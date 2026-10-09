//! Startup settings. File values are overridden by SEPTEMBER_* environment values.

use std::{env, net::SocketAddr, path::Path};

use config::{Config, Environment, File};
use september::summarizer::Provider;
use serde::Deserialize;

/// What builds summaries: a model, or nothing, leaving jobs to external workers.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Summarizer {
    None,
    #[serde(untagged)]
    Model(Provider),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub bind: SocketAddr,
    pub summarizer: Summarizer,
    pub model: Option<String>,
}

/// A startup failure. Messages are fixed so that setting values are never echoed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Error(&'static str);

impl Settings {
    pub(crate) fn load() -> Result<Self, Error> {
        let path = env::var_os("SEPTEMBER_CONFIG");
        // Remove the file selector before deserializing the settings themselves.
        let environment = env::vars_os()
            .filter(|(key, _)| {
                key.to_str()
                    .is_some_and(|key| key.starts_with("SEPTEMBER_") && key != "SEPTEMBER_CONFIG")
            })
            .map(|(key, value)| {
                Ok((
                    key.into_string()
                        .map_err(|_| Error("invalid settings variable name"))?,
                    value
                        .into_string()
                        .map_err(|_| Error("settings variables must contain UTF-8"))?,
                ))
            })
            .collect::<Result<_, Error>>()?;
        Self::read(
            path.as_deref().map(Path::new),
            Environment::with_prefix("SEPTEMBER").source(Some(environment)),
        )
    }

    fn read(path: Option<&Path>, environment: Environment) -> Result<Self, Error> {
        let mut builder = Config::builder()
            .set_default("bind", "127.0.0.1:3000")
            .and_then(|builder| builder.set_default("summarizer", "none"))
            .map_err(|_| Error("could not initialize settings"))?;
        if let Some(path) = path {
            builder = builder.add_source(File::from(path).format(config::FileFormat::Toml));
        }
        // Config errors can include raw values; never report their contents.
        let settings: Self = builder
            .add_source(environment)
            .build()
            .and_then(Config::try_deserialize)
            .map_err(|_| {
                Error("invalid settings: check SEPTEMBER_CONFIG and SEPTEMBER_* variables")
            })?;
        if !settings.bind.ip().is_loopback() {
            return Err(Error("the unauthenticated server must bind to loopback"));
        }
        if matches!(settings.summarizer, Summarizer::Model(_))
            && settings
                .model
                .as_ref()
                .is_none_or(|model| model.trim().is_empty())
        {
            return Err(Error("SEPTEMBER_MODEL is required for real summarization"));
        }
        Ok(settings)
    }
}

/// A provider's standalone API key. Harness logins are never consulted.
pub(crate) fn api_key(provider: Provider) -> Result<String, Error> {
    let (name, missing) = match provider {
        Provider::Openai => ("OPENAI_API_KEY", "OPENAI_API_KEY is required"),
        Provider::Anthropic => ("ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY is required"),
    };
    env::var(name)
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or(Error(missing))
}

#[cfg(test)]
mod tests;
