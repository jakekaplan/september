use rusqlite::{OptionalExtension, TransactionBehavior, params};
use september_memory::Node;
use tokio::time::Instant;
use uuid::Uuid;

use super::{Sqlite, State, load_views, message, publish, summary};
use crate::{
    Error,
    jobs::{Claim, Completion, Context, Input, Job, LEASE},
    storage::Jobs,
};

impl Jobs for Sqlite {
    async fn claim(&self) -> Result<Option<Claim>, Error> {
        let now = Instant::now();
        self.run(move |state| state.claim(now)).await
    }

    async fn renew(&self, node: Node, token: Uuid) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        state.claims.renew(node, token, Instant::now())
    }

    async fn complete(&self, completion: Completion) -> Result<(), Error> {
        completion.validate()?;
        let node = Node::try_from(completion.range)?;
        let now = Instant::now();
        self.run(move |state| state.complete(node, completion, now))
            .await
    }
}

impl State {
    fn claim(&mut self, now: Instant) -> Result<Option<Claim>, Error> {
        self.claims.expire(now);
        if self.claims.is_full() {
            return Ok(None);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let jobs: Vec<(u64, u64, Option<u64>, Option<String>)> = tx
            .prepare_cached(
                "SELECT start, length, context_cutoff, context_view FROM jobs ORDER BY seq",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut views = None;
        let mut selected = None;
        for (start, length, cutoff, view) in jobs {
            let node = Node::new(start, length)?;
            if self.claims.is_claimed(node) {
                continue;
            }
            let context = if let (Some(cutoff), Some(view)) = (cutoff, view) {
                Some(Context { cutoff, view })
            } else {
                let views = match &views {
                    Some(views) => views,
                    None => views.insert(load_views(&tx, self.budget)?),
                };
                // Let other ready parents shrink the context before admitting this job.
                Context::select(views, node)?
            };
            if let Some(context) = context {
                selected = Some((node, context));
                break;
            }
        }
        let Some((node, context)) = selected else {
            return Ok(None);
        };
        let input = if let Some([left, right]) = node.children() {
            Input::Children {
                summaries: [summary(&tx, left)?, summary(&tx, right)?],
            }
        } else {
            Input::Message {
                message: message(&tx, node.start())?,
            }
        };
        tx.prepare_cached(
            "UPDATE jobs SET context_cutoff = ?3, context_view = ?4
             WHERE start = ?1 AND length = ?2",
        )?
        .execute(params![
            node.start(),
            node.length(),
            context.cutoff,
            context.view
        ])?;
        tx.commit()?;
        let token = self.claims.grant(node, now);
        Ok(Some(Claim {
            job: Job {
                range: node.into(),
                input,
                context,
            },
            token,
            lease_seconds: LEASE.as_secs(),
        }))
    }

    fn complete(&mut self, node: Node, completion: Completion, now: Instant) -> Result<(), Error> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let published: Option<(String, Option<String>)> = tx
            .prepare_cached("SELECT text, token FROM summaries WHERE start = ?1 AND length = ?2")?
            .query_row([node.start(), node.length()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some((text, token)) = published {
            return if text == completion.text && token == Some(completion.token.to_string()) {
                Ok(())
            } else {
                Err(Error::ClaimLost)
            };
        }
        self.claims.check(node, completion.token, now)?;
        let summary = september_memory::Summary::new(node, completion.text);
        let published = publish(&tx, self.budget, summary, Some(completion.token))?;
        tx.commit()?;
        self.release(&published);
        Ok(())
    }
}
