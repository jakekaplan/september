//! Startup settings. File values are overridden by SEPTEMBER_* environment values.

use std::{env, fmt, net::SocketAddr, path::Path};

use config::{Config, Environment, File};
use serde::Deserialize;

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Summarizer {
    None,
    Fake,
    Openai,
    Anthropic,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub bind: SocketAddr,
    storage: String,
    pub summarizer: Summarizer,
    pub model: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Error(&'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Error {}

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
            .and_then(|builder| builder.set_default("storage", "memory"))
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
        if settings.storage != "memory" {
            return Err(Error("SEPTEMBER_STORAGE currently supports only memory"));
        }
        if !settings.bind.ip().is_loopback() {
            return Err(Error("the unauthenticated server must bind to loopback"));
        }
        if matches!(
            settings.summarizer,
            Summarizer::Openai | Summarizer::Anthropic
        ) && settings
            .model
            .as_ref()
            .is_none_or(|model| model.trim().is_empty())
        {
            return Err(Error("SEPTEMBER_MODEL is required for real summarization"));
        }
        Ok(settings)
    }

    pub(crate) fn api_key(&self) -> Result<String, Error> {
        let name = match self.summarizer {
            Summarizer::Openai => "OPENAI_API_KEY",
            Summarizer::Anthropic => "ANTHROPIC_API_KEY",
            _ => return Err(Error("the selected summarizer does not use an API key")),
        };
        env::var(name)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or(Error(match self.summarizer {
                Summarizer::Openai => "OPENAI_API_KEY is required",
                _ => "ANTHROPIC_API_KEY is required",
            }))
    }
}

#[cfg(test)]
mod tests;
