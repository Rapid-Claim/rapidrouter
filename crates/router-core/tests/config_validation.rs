//! Table tests: every class of invalid config must be rejected with a
//! pathed error, and valid configs must resolve fully.

use std::collections::BTreeMap;

use router_core::config::{AuthMode, Config, Format, LoadError, ProviderKind};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |var: &str| map.get(var).cloned()
}

fn load(toml: &str, env_pairs: &[(&str, &str)]) -> Result<Config, LoadError> {
    Config::from_str_with_env(toml, Format::Toml, &env(env_pairs))
}

/// Assert the config fails validation with an error at `path` whose
/// message contains `fragment`.
fn assert_invalid(toml: &str, env_pairs: &[(&str, &str)], path: &str, fragment: &str) {
    match load(toml, env_pairs) {
        Err(LoadError::Invalid(errors)) => {
            assert!(
                errors
                    .iter()
                    .any(|e| e.path == path && e.message.contains(fragment)),
                "expected error at `{path}` containing `{fragment}`, got: {errors:?}"
            );
        }
        Err(other) => panic!("expected validation error, got: {other}"),
        Ok(_) => panic!("expected validation error at `{path}`, config was accepted"),
    }
}

const FULL: &str = r#"
[server]
port = 9090
auth_keys = ["env.GATEWAY_KEY"]

[providers.openai]
keys = [
  { name = "primary", value = "env.OPENAI_API_KEY", weight = 0.7, models = ["gpt-4o"] },
  { name = "secondary", value = "env.OPENAI_API_KEY_2", weight = 0.3 },
]

[providers.groq]
keys = [{ name = "main", value = "env.GROQ_API_KEY" }]

[providers.internal]
type = "openai_compat"
base_url = "https://llm.internal.example/v1"
keys = [{ name = "main", value = "env.INTERNAL_KEY" }]

[providers.ollama]
auth = "none"

[providers.azure]
endpoint = "https://myres.openai.azure.com"
api_version = "2024-10-21"
keys = [{ name = "main", value = "env.AZURE_KEY" }]
[providers.azure.deployments]
"gpt-4o" = "my-gpt4o"

[aliases]
fast = "groq/llama-3.3-70b"
smart = "fast"

[fallbacks]
"openai/gpt-4o" = ["azure/gpt-4o", "fast"]
"#;

const FULL_ENV: &[(&str, &str)] = &[
    ("GATEWAY_KEY", "ck-test"),
    ("OPENAI_API_KEY", "sk-1"),
    ("OPENAI_API_KEY_2", "sk-2"),
    ("GROQ_API_KEY", "gsk-1"),
    ("INTERNAL_KEY", "ik-1"),
    ("AZURE_KEY", "az-1"),
];

#[test]
fn full_valid_config_resolves() {
    let config = load(FULL, FULL_ENV).expect("config should be valid");

    assert_eq!(config.server.port, 9090);
    assert_eq!(config.server.auth_keys.len(), 1);
    assert!(config.server.auth_keys[0].verify("ck-test"));

    let openai = &config.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenAi);
    assert_eq!(openai.keys.len(), 2);
    assert_eq!(
        openai.keys[0].models.as_deref(),
        Some(&["gpt-4o".to_string()][..])
    );
    assert!(openai.keys[0].secret.verify("sk-1"));

    assert_eq!(config.providers["groq"].kind, ProviderKind::OpenAiCompat);
    assert!(
        config.providers["groq"]
            .base_url
            .as_deref()
            .unwrap()
            .contains("groq.com")
    );
    assert_eq!(config.providers["ollama"].auth, AuthMode::None);
    assert_eq!(
        config.providers["azure"]
            .azure
            .as_ref()
            .unwrap()
            .deployments["gpt-4o"],
        "my-gpt4o"
    );

    // Alias chain resolves through `fast` to the terminal target.
    assert_eq!(config.aliases["smart"].to_string(), "groq/llama-3.3-70b");

    // Fallback chain resolved, including the alias entry.
    let chain =
        &config.fallbacks[&router_core::config::TargetModel::parse("openai/gpt-4o").unwrap()];
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].to_string(), "groq/llama-3.3-70b");
}

#[test]
fn empty_config_is_valid_but_bare() {
    let config = load("", &[]).expect("empty config is structurally valid");
    assert!(config.providers.is_empty());
    assert_eq!(config.server.port, 8080);
}

#[test]
fn unknown_field_rejected() {
    let err = load("[server]\nprot = 8080\n", &[]).unwrap_err();
    assert!(
        matches!(err, LoadError::Parse(_)),
        "unknown fields must fail parsing: {err}"
    );
}

#[test]
fn missing_env_var_is_pathed_error() {
    assert_invalid(
        "[providers.openai]\nkeys = [{ name = \"a\", value = \"env.NOPE\" }]\n",
        &[],
        "providers.openai.keys[0].value",
        "`NOPE` is not set",
    );
}

#[test]
fn empty_env_var_rejected() {
    assert_invalid(
        "[providers.openai]\nkeys = [{ name = \"a\", value = \"env.EMPTY\" }]\n",
        &[("EMPTY", "")],
        "providers.openai.keys[0].value",
        "set but empty",
    );
}

#[test]
fn store_secrets_not_yet_available() {
    assert_invalid(
        "[providers.openai]\nkeys = [{ name = \"a\", value = \"store.k\" }]\n",
        &[],
        "providers.openai.keys[0].value",
        "managed mode",
    );
}

#[test]
fn zero_and_negative_weights_rejected() {
    for weight in ["0.0", "-1.0"] {
        assert_invalid(
            &format!(
                "[providers.openai]\nkeys = [{{ name = \"a\", value = \"env.K\", weight = {weight} }}]\n"
            ),
            &[("K", "sk")],
            "providers.openai.keys[0].weight",
            "must be > 0",
        );
    }
}

#[test]
fn duplicate_key_names_rejected() {
    assert_invalid(
        r#"
[providers.openai]
keys = [
  { name = "a", value = "env.K" },
  { name = "a", value = "env.K" },
]
"#,
        &[("K", "sk")],
        "providers.openai.keys[1].name",
        "duplicate",
    );
}

#[test]
fn provider_without_keys_rejected() {
    assert_invalid(
        "[providers.openai]\n",
        &[],
        "providers.openai.keys",
        "at least one key",
    );
}

#[test]
fn unknown_provider_needs_type_and_base_url() {
    assert_invalid(
        "[providers.mystery]\nkeys = [{ name = \"a\", value = \"env.K\" }]\n",
        &[("K", "sk")],
        "providers.mystery",
        "not a well-known provider",
    );
    assert_invalid(
        r#"
[providers.mystery]
type = "openai_compat"
keys = [{ name = "a", value = "env.K" }]
"#,
        &[("K", "sk")],
        "providers.mystery.base_url",
        "required",
    );
}

#[test]
fn bad_base_url_scheme_rejected() {
    assert_invalid(
        r#"
[providers.internal]
type = "openai_compat"
base_url = "ftp://nope"
keys = [{ name = "a", value = "env.K" }]
"#,
        &[("K", "sk")],
        "providers.internal.base_url",
        "http",
    );
}

#[test]
fn azure_requires_endpoint_and_api_version() {
    assert_invalid(
        "[providers.azure]\nkeys = [{ name = \"a\", value = \"env.K\" }]\n",
        &[("K", "sk")],
        "providers.azure.endpoint",
        "required",
    );
    assert_invalid(
        "[providers.azure]\nkeys = [{ name = \"a\", value = \"env.K\" }]\n",
        &[("K", "sk")],
        "providers.azure.api_version",
        "required",
    );
}

#[test]
fn azure_fields_rejected_elsewhere() {
    assert_invalid(
        r#"
[providers.openai]
endpoint = "https://x"
keys = [{ name = "a", value = "env.K" }]
"#,
        &[("K", "sk")],
        "providers.openai.endpoint",
        "only valid for the azure provider",
    );
}

#[test]
fn alias_cycle_rejected() {
    assert_invalid(
        "[aliases]\na = \"b\"\nb = \"a\"\n",
        &[],
        "aliases.a",
        "cycle",
    );
}

#[test]
fn alias_to_unknown_provider_rejected() {
    assert_invalid(
        "[aliases]\nfast = \"nope/model\"\n",
        &[],
        "aliases.fast",
        "unknown provider",
    );
}

#[test]
fn fallback_to_unknown_target_rejected() {
    assert_invalid(
        r#"
[providers.openai]
keys = [{ name = "a", value = "env.K" }]

[fallbacks]
"openai/gpt-4o" = ["nope/gpt-4o"]
"#,
        &[("K", "sk")],
        "fallbacks.openai/gpt-4o[0]",
        "unknown provider",
    );
}

#[test]
fn fallback_to_self_rejected() {
    assert_invalid(
        r#"
[providers.openai]
keys = [{ name = "a", value = "env.K" }]

[fallbacks]
"openai/gpt-4o" = ["openai/gpt-4o"]
"#,
        &[("K", "sk")],
        "fallbacks.openai/gpt-4o[0]",
        "equals its own source",
    );
}

#[test]
fn all_errors_reported_at_once() {
    let result = load(
        r#"
[providers.openai]
keys = [{ name = "", value = "env.MISSING", weight = 0.0 }]

[aliases]
bad = "nope/model"
"#,
        &[],
    );
    let Err(LoadError::Invalid(errors)) = result else {
        panic!("expected validation errors");
    };
    assert!(
        errors.len() >= 4,
        "expected all errors collected, got: {errors:?}"
    );
}

#[test]
fn json_config_accepted() {
    let json = r#"{
      "providers": {
        "openai": { "keys": [{ "name": "a", "value": "env.K" }] }
      }
    }"#;
    let config = Config::from_str_with_env(json, Format::Json, &env(&[("K", "sk")])).unwrap();
    assert_eq!(config.providers["openai"].kind, ProviderKind::OpenAi);
}

#[test]
fn discovery_builds_providers_from_env() {
    let source = env(&[("OPENAI_API_KEY", "sk-1"), ("GROQ_API_KEY", "gsk-1")]);
    let config = Config::discover_from_env(&source).expect("two providers discoverable");
    assert_eq!(config.providers.len(), 2);
    assert!(config.providers.contains_key("openai"));
    assert!(config.providers.contains_key("groq"));
    assert!(config.providers["openai"].keys[0].secret.verify("sk-1"));
}

#[test]
fn discovery_with_no_env_is_none() {
    let source = env(&[]);
    assert!(Config::discover_from_env(&source).is_none());
}

#[test]
fn secrets_never_appear_in_debug_output() {
    let config = load(FULL, FULL_ENV).unwrap();
    let debug = format!("{config:?}");
    for secret in ["sk-1", "sk-2", "gsk-1", "ik-1", "az-1", "ck-test"] {
        assert!(
            !debug.contains(secret),
            "secret `{secret}` leaked into Debug output"
        );
    }
    assert!(debug.contains("[REDACTED]"));
}
