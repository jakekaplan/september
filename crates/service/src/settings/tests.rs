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
    assert_eq!(settings.queue, "memory://september");
}

#[test]
fn validates_startup_settings_without_echoing_values() {
    for values in [
        vec![("SUMMARIZER", "secret-invalid-provider")],
        vec![("SUMMARIZER", "openai")],
        vec![("SUMMARIZER", "fake")],
        vec![("SUMMARIZER", "anthropic"), ("MODEL", " ")],
        vec![("BIND", "0.0.0.0:3000")],
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
        environment(&[
            ("SUMMARIZER", "anthropic"),
            ("MODEL", "from-env"),
            ("QUEUE", "redis://localhost:6379/0"),
        ]),
    )
    .unwrap();
    fs::remove_file(&path).unwrap();
    assert_eq!(settings.summarizer, Summarizer::Model(Provider::Anthropic));
    assert_eq!(settings.model.as_deref(), Some("from-env"));
    assert_eq!(settings.queue, "redis://localhost:6379/0");
    assert_eq!(settings.bind.port(), 4000);
    assert!(Settings::read(Some(&path), environment(&[])).is_err());
}

#[test]
fn unknown_september_variables_are_ignored_by_name_but_settings_are_kept() {
    let (settings, ignored) = variables(
        [
            ("SEPTEMBER_MODEL", "from-env"),
            ("SEPTEMBER_URL", "http://127.0.0.1:3000"),
            ("SEPTEMBER_CONFIG", "/tmp/september.toml"),
            ("PATH", "/usr/bin"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into())),
    )
    .unwrap();
    assert_eq!(
        settings,
        HashMap::from([("SEPTEMBER_MODEL".to_owned(), "from-env".to_owned())])
    );
    assert_eq!(ignored, ["SEPTEMBER_URL"]);
}

#[test]
fn the_settings_file_still_rejects_unknown_keys() {
    let path = std::env::temp_dir().join(format!("september-{}.toml", uuid::Uuid::new_v4()));
    fs::write(&path, "storage = 'memory'\n").unwrap();
    let result = Settings::read(Some(&path), environment(&[]));
    fs::remove_file(&path).unwrap();
    assert!(result.is_err());
}
