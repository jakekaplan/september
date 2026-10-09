use std::{collections::HashMap, fs};

use super::*;

fn environment(values: &[(&str, &str)]) -> Environment {
    Environment::with_prefix("SEPTEMBER").source(Some(
        values
            .iter()
            .map(|(key, value)| (format!("SEPTEMBER_{key}"), (*value).to_owned()))
            .collect::<HashMap<_, _>>(),
    ))
}

#[test]
fn defaults_need_no_model_or_credentials() {
    let settings = Settings::read(None, environment(&[])).unwrap();
    assert_eq!(settings.bind.to_string(), "127.0.0.1:3000");
    assert_eq!(settings.summarizer, Summarizer::None);
    assert!(settings.model.is_none());
}

#[test]
fn validates_startup_settings_without_echoing_values() {
    for values in [
        vec![("SUMMARIZER", "secret-invalid-provider")],
        vec![("SUMMARIZER", "openai")],
        vec![("SUMMARIZER", "fake")],
        vec![("SUMMARIZER", "anthropic"), ("MODEL", " ")],
        vec![("BIND", "0.0.0.0:3000")],
        vec![("STORAGE", "memory")],
        vec![("API_KEY", "secret-invalid-provider")],
    ] {
        let result = Settings::read(None, environment(&values));
        assert!(result.is_err());
        assert!(
            !result
                .err()
                .unwrap()
                .to_string()
                .contains("secret-invalid-provider")
        );
    }
}

#[test]
fn environment_overrides_toml_and_missing_explicit_file_fails() {
    let path = std::env::temp_dir().join(format!("september-{}.toml", uuid::Uuid::new_v4()));
    fs::write(
        &path,
        "summarizer = 'openai'\nmodel = 'from-file'\nbind = '127.0.0.1:4000'\n",
    )
    .unwrap();
    let settings = Settings::read(
        Some(&path),
        environment(&[("SUMMARIZER", "anthropic"), ("MODEL", "from-env")]),
    )
    .unwrap();
    fs::remove_file(&path).unwrap();
    assert_eq!(settings.summarizer, Summarizer::Model(Provider::Anthropic));
    assert_eq!(settings.model.as_deref(), Some("from-env"));
    assert_eq!(settings.bind.port(), 4000);
    assert!(Settings::read(Some(&path), environment(&[])).is_err());
}
