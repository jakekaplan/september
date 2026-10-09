//! Startup settings. File values are overridden by SEPTEMBER_* environment values.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
};

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

/// Where the archive lives: in process memory, lost at shutdown, or in SQLite.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Storage {
    Memory,
    Sqlite,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub bind: SocketAddr,
    pub summarizer: Summarizer,
    pub model: Option<String>,
    /// Docket's queue: `memory://` in process, or a Redis URL.
    pub queue: String,
    pub storage: Storage,
    /// The SQLite file, created if missing, when `storage` is `sqlite`.
    pub database: PathBuf,
}

/// A startup failure. Messages are fixed so that setting values are never echoed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Error(&'static str);

impl Settings {
    pub(crate) fn load() -> Result<Self, Error> {
        let path = env::var_os("SEPTEMBER_CONFIG");
        let (environment, ignored) = variables(env::vars_os())?;
        if !ignored.is_empty() {
            tracing::warn!(
                ?ignored,
                "ignoring SEPTEMBER_* variables that are not server settings"
            );
        }
        Self::read(
            path.as_deref().map(Path::new),
            Environment::with_prefix("SEPTEMBER").source(Some(environment)),
        )
    }

    fn read(path: Option<&Path>, environment: Environment) -> Result<Self, Error> {
        let mut builder = Config::builder()
            .set_default("bind", "127.0.0.1:3000")
            .and_then(|builder| builder.set_default("summarizer", "none"))
            .and_then(|builder| builder.set_default("queue", "memory://september"))
            .and_then(|builder| builder.set_default("storage", "memory"))
            .and_then(|builder| builder.set_default("database", "data/september.sqlite3"))
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

/// The server settings among `SEPTEMBER_*` variables, and the names of the others.
///
/// Clients share the prefix (the Claude Code plugin reads `SEPTEMBER_URL`), so an
/// unknown name is reported rather than fatal. The settings file stays strict.
fn variables(
    environment: impl Iterator<Item = (OsString, OsString)>,
) -> Result<(HashMap<String, String>, Vec<String>), Error> {
    const SETTINGS: [&str; 6] = [
        "SEPTEMBER_BIND",
        "SEPTEMBER_SUMMARIZER",
        "SEPTEMBER_MODEL",
        "SEPTEMBER_QUEUE",
        "SEPTEMBER_STORAGE",
        "SEPTEMBER_DATABASE",
    ];
    let mut settings = HashMap::new();
    let mut ignored = Vec::new();
    for (key, value) in environment {
        let Some(key) = key.to_str() else { continue };
        // SEPTEMBER_CONFIG selects the settings file; it is not a setting itself.
        if !key.starts_with("SEPTEMBER_") || key == "SEPTEMBER_CONFIG" {
            continue;
        }
        if !SETTINGS.contains(&key) {
            ignored.push(key.to_owned());
            continue;
        }
        let value = value
            .into_string()
            .map_err(|_| Error("settings variables must contain UTF-8"))?;
        settings.insert(key.to_owned(), value);
    }
    Ok((settings, ignored))
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
