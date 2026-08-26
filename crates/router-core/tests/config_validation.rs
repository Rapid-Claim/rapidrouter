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
fn default_upload_limit_is_one_hundred_megabytes() {
    let config = Config::from_str("", Format::Toml).unwrap();
    assert_eq!(config.server.max_body_size, 100 * 1024 * 1024);
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

#[test]
fn bedrock_requires_region_and_credentials() {
    assert_invalid(
        "[providers.bedrock]\nkeys = [{ name = \"k\", value = \"secret\" }]\n",
        &[],
        "providers.bedrock.region",
        "required",
    );
    assert_invalid(
        "[providers.bedrock]\nkeys = [{ name = \"k\", value = \"secret\" }]\n",
        &[],
        "providers.bedrock.access_key_id",
        "required",
    );
}

#[test]
fn vertex_requires_project_and_derives_base_url() {
    assert_invalid(
        "[providers.vertex]\nkeys = [{ name = \"k\", value = \"tok\" }]\n",
        &[],
        "providers.vertex.project",
        "required",
    );
    let config = load(
        "[providers.vertex]\nproject = \"p1\"\nkeys = [{ name = \"k\", value = \"tok\" }]\n",
        &[],
    )
    .unwrap();
    assert_eq!(
        config.providers["vertex"].base_url.as_deref(),
        Some("https://us-central1-aiplatform.googleapis.com")
    );
}

#[test]
fn region_rejected_outside_bedrock() {
    assert_invalid(
        "[providers.openai]\nregion = \"us-east-1\"\nkeys = [{ name = \"k\", value = \"sk\" }]\n",
        &[],
        "providers.openai.region",
        "only valid for the bedrock provider",
    );
}

// ---- Phase 7 sections: virtual keys, console, usage, pricing ----

const VK_HASH: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

fn vk_block(extra: &str) -> String {
    format!(
        "[providers.openai]\nkeys = [{{ name = \"k\", value = \"sk\" }}]\n\n\
         [[virtual_keys]]\nname = \"svc\"\nid = \"9f3a2c\"\nsecret_hash = \"{VK_HASH}\"\n{extra}"
    )
}

#[test]
fn virtual_key_full_form_resolves() {
    let config = load(
        &vk_block(
            "models = [\"openai/gpt-4o-mini\", \"fast\"]\n\
             budget = { usd = 250.0, period = \"monthly\" }\n\
             rate_limit = { rpm = 600, tpm = 400000 }\n\
             expires = \"2027-01-01T00:00:00Z\"\n\
             tags = { team = \"payments\" }\n\n\
             [aliases]\nfast = \"openai/gpt-4o-mini\"\n",
        ),
        &[],
    )
    .unwrap();
    let vk = &config.virtual_keys[0];
    assert_eq!(vk.id, "9f3a2c");
    assert_eq!(vk.models, vec!["openai/gpt-4o-mini", "fast"]);
    assert!(vk.budget.is_some());
    assert_eq!(vk.rate.unwrap().rpm, Some(600));
    assert!(vk.expires_ms.is_some());
    assert!(vk.enabled);
}

#[test]
fn virtual_key_bad_id_and_hash_rejected() {
    assert_invalid(
        "[[virtual_keys]]\nname = \"a\"\nid = \"xyz\"\nsecret_hash = \"blake3:00\"\n",
        &[],
        "virtual_keys[0].id",
        "6 hex characters",
    );
    assert_invalid(
        "[[virtual_keys]]\nname = \"a\"\nid = \"9f3a2c\"\nsecret_hash = \"plain\"\n",
        &[],
        "virtual_keys[0].secret_hash",
        "blake3:",
    );
}

#[test]
fn virtual_key_duplicate_ids_rejected() {
    let doc = format!(
        "[[virtual_keys]]\nname = \"a\"\nid = \"9f3a2c\"\nsecret_hash = \"{VK_HASH}\"\n\n\
         [[virtual_keys]]\nname = \"b\"\nid = \"9f3a2c\"\nsecret_hash = \"{VK_HASH}\"\n"
    );
    assert_invalid(&doc, &[], "virtual_keys[1].id", "duplicate");
}

const TENANTS: &str = "tenants = [\"agi\", \"optimizer\"]\n";

#[test]
fn a_key_names_the_service_it_belongs_to() {
    let config = load(&format!("{TENANTS}{}", vk_block("tenant = \"agi\"\n")), &[]).unwrap();
    assert_eq!(config.virtual_keys[0].tenant.as_deref(), Some("agi"));
    assert!(config.tenants.contains("optimizer"));
}

#[test]
fn a_key_may_not_name_a_service_nobody_declared() {
    assert_invalid(
        &vk_block("tenant = \"ghost\"\n"),
        &[],
        "virtual_keys[0].tenant",
        "no service named `ghost` is declared",
    );
}

/// A typo on an account would leave it owned by nobody, serving nobody.
#[test]
fn an_account_may_not_name_a_service_nobody_declared() {
    assert_invalid(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\", tenant = \"ghost\" }]\n",
        &[],
        "providers.openai.keys[0].tenant",
        "no service named `ghost` is declared",
    );
}

#[test]
fn duplicate_service_names_are_rejected() {
    assert_invalid(
        "tenants = [\"agi\", \"agi\"]\n",
        &[],
        "tenants[1]",
        "duplicate service `agi`",
    );
}

#[test]
fn account_labels_resolve_onto_the_provider() {
    let config = load(
        &format!(
            "{TENANTS}[providers.openai]\nkeys = [\n\
               {{ name = \"a\", value = \"sk\", tenant = \"agi\" }},\n\
               {{ name = \"b\", value = \"sk\" }},\n\
             ]\n"
        ),
        &[],
    )
    .unwrap();
    let keys = &config.providers["openai"].keys;
    assert_eq!(keys[0].tenant.as_deref(), Some("agi"));
    assert_eq!(keys[1].tenant, None, "unassigned");
}

#[test]
fn virtual_key_scope_must_name_known_provider_or_alias() {
    assert_invalid(
        &vk_block("models = [\"nosuch/model\"]\n"),
        &[],
        "virtual_keys[0].models[0]",
        "unknown provider",
    );
    assert_invalid(
        &vk_block("models = [\"bare-model\"]\n"),
        &[],
        "virtual_keys[0].models[0]",
        "not a configured routing group or alias",
    );
}

#[test]
fn virtual_key_budget_and_rate_bounds() {
    assert_invalid(
        &vk_block("budget = { usd = -1.0, period = \"monthly\" }\n"),
        &[],
        "virtual_keys[0].budget.usd",
        "positive",
    );
    assert_invalid(
        &vk_block("budget = { usd = 1.0, period = \"hourly\" }\n"),
        &[],
        "virtual_keys[0].budget.period",
        "unknown period",
    );
    assert_invalid(
        &vk_block("rate_limit = { rpm = 0 }\n"),
        &[],
        "virtual_keys[0].rate_limit.rpm",
        "must be > 0",
    );
    assert_invalid(
        &vk_block("rate_limit = {}\n"),
        &[],
        "virtual_keys[0].rate_limit",
        "rpm and/or tpm",
    );
    assert_invalid(
        &vk_block("expires = \"tomorrow\"\n"),
        &[],
        "virtual_keys[0].expires",
        "RFC 3339",
    );
}

#[test]
fn console_admin_keys_resolve_and_ttl_is_bounded() {
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\" }]\n\n\
         [console]\nadmin_keys = [\"env.ADMIN_KEY\"]\n",
        &[("ADMIN_KEY", "admin-secret")],
    )
    .unwrap();
    assert!(config.console.enabled());
    assert!(config.console.admin_keys[0].verify("admin-secret"));

    assert_invalid(
        "[console]\nadmin_keys = [\"env.MISSING\"]\n",
        &[],
        "console.admin_keys[0]",
        "not set",
    );
    assert_invalid(
        "[console]\nsession_ttl_secs = 5\n",
        &[],
        "console.session_ttl_secs",
        "between",
    );
}

#[test]
fn usage_and_pricing_bounds() {
    assert_invalid(
        "[usage]\nretention_days = 0\n",
        &[],
        "usage.retention_days",
        "between",
    );
    assert_invalid(
        "[pricing.\"openai/gpt-4o-mini\"]\ninput_per_mtok = -0.5\noutput_per_mtok = 0.6\n",
        &[],
        "pricing.openai/gpt-4o-mini",
        "non-negative",
    );
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\" }]\n\n\
         [pricing.\"openai/gpt-4o-mini\"]\ninput_per_mtok = 0.15\noutput_per_mtok = 0.6\n",
        &[],
    )
    .unwrap();
    assert_eq!(config.pricing["openai/gpt-4o-mini"].input_per_mtok, 0.15);
}

#[test]
fn store_refs_resolve_through_the_source() {
    // Managed mode composes a source that answers `store.<name>`.
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"store.openai_key\" }]\n",
        &[("store.openai_key", "sk-from-store")],
    )
    .unwrap();
    assert_eq!(
        config.providers["openai"].keys[0].secret.expose(),
        "sk-from-store"
    );

    assert_invalid(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"store.missing\" }]\n",
        &[],
        "providers.openai.keys[0].value",
        "store secret `missing` is not set",
    );
}

// --------------------------------------------------------- routing groups

const TWO_PROVIDERS: &str = r#"
[providers.openai]
keys = [{ name = "a", value = "k", models = ["gpt-4o-mini"] }]

[providers.groq]
keys = [{ name = "a", value = "k", models = ["llama-3.3-70b"] }]
"#;

#[test]
fn routing_group_resolves_both_pools() {
    let config = load(
        &format!(
            r#"{TWO_PROVIDERS}
[groups.fast]
primary = [
  {{ target = "openai/gpt-4o-mini", weight = 3 }},
  {{ target = "groq/llama-3.3-70b", weight = 1 }},
]
fallback = [{{ target = "groq/llama-3.3-70b" }}]
"#
        ),
        &[],
    )
    .expect("valid group config");

    let group = &config.groups["fast"];
    assert_eq!(group.primary.len(), 2);
    assert_eq!(group.primary[0].target.to_string(), "openai/gpt-4o-mini");
    assert_eq!(group.primary[0].weight, 3.0);
    // An omitted weight is an equal share, not zero.
    assert_eq!(group.fallback[0].weight, 1.0);
}

#[test]
fn routing_group_resolves_alias_targets() {
    let config = load(
        &format!(
            r#"{TWO_PROVIDERS}
[aliases]
cheap = "groq/llama-3.3-70b"

[groups.fast]
primary = [{{ target = "cheap" }}]
"#
        ),
        &[],
    )
    .expect("alias target is a valid group member");
    assert_eq!(
        config.groups["fast"].primary[0].target.to_string(),
        "groq/llama-3.3-70b"
    );
}

#[test]
fn routing_group_without_primary_rejected() {
    assert_invalid(
        &format!(
            "{TWO_PROVIDERS}\n[groups.fast]\nfallback = [{{ target = \"openai/gpt-4o-mini\" }}]\n"
        ),
        &[],
        "groups.fast.primary",
        "at least one primary model",
    );
}

#[test]
fn routing_group_zero_weight_rejected() {
    assert_invalid(
        &format!(
            "{TWO_PROVIDERS}\n[groups.fast]\nprimary = [{{ target = \"openai/gpt-4o-mini\", weight = 0 }}]\n"
        ),
        &[],
        "groups.fast.primary[0].weight",
        "> 0",
    );
}

#[test]
fn routing_group_unknown_target_rejected() {
    assert_invalid(
        &format!("{TWO_PROVIDERS}\n[groups.fast]\nprimary = [{{ target = \"nope/model\" }}]\n"),
        &[],
        "groups.fast.primary[0].target",
        "unknown provider",
    );
}

#[test]
fn routing_group_duplicate_target_in_pool_rejected() {
    assert_invalid(
        &format!(
            r#"{TWO_PROVIDERS}
[groups.fast]
primary = [
  {{ target = "openai/gpt-4o-mini" }},
  {{ target = "openai/gpt-4o-mini", weight = 2 }},
]
"#
        ),
        &[],
        "groups.fast.primary[1].target",
        "already in this group's primary pool",
    );
}

#[test]
fn routing_group_may_repeat_a_primary_model_as_fallback() {
    // The pools are separate questions: a model can carry live traffic
    // and also be the thing another provider's failure falls back to.
    let config = load(
        &format!(
            r#"{TWO_PROVIDERS}
[groups.fast]
primary = [{{ target = "openai/gpt-4o-mini" }}]
fallback = [{{ target = "openai/gpt-4o-mini" }}]
"#
        ),
        &[],
    )
    .expect("same target in both pools is allowed");
    assert_eq!(config.groups["fast"].fallback.len(), 1);
}

#[test]
fn routing_group_naming_a_group_rejected() {
    assert_invalid(
        &format!(
            r#"{TWO_PROVIDERS}
[groups.fast]
primary = [{{ target = "openai/gpt-4o-mini" }}]

[groups.nested]
primary = [{{ target = "fast" }}]
"#
        ),
        &[],
        "groups.nested.primary[0].target",
        "is a routing group",
    );
}

#[test]
fn routing_group_colliding_with_alias_rejected() {
    assert_invalid(
        &format!(
            r#"{TWO_PROVIDERS}
[aliases]
fast = "groq/llama-3.3-70b"

[groups.fast]
primary = [{{ target = "openai/gpt-4o-mini" }}]
"#
        ),
        &[],
        "groups.fast",
        "collides with an alias",
    );
}

#[test]
fn routing_group_colliding_with_provider_rejected() {
    assert_invalid(
        &format!(
            "{TWO_PROVIDERS}\n[groups.openai]\nprimary = [{{ target = \"openai/gpt-4o-mini\" }}]\n"
        ),
        &[],
        "groups.openai",
        "collides with a provider name",
    );
}

#[test]
fn virtual_key_may_scope_to_a_routing_group() {
    load(
        &format!(
            r#"{TWO_PROVIDERS}
[groups.fast]
primary = [{{ target = "openai/gpt-4o-mini" }}]

[[virtual_keys]]
name = "team"
id = "a1b2c3"
secret_hash = "blake3:{}"
models = ["fast"]
"#,
            "0".repeat(64)
        ),
        &[],
    )
    .expect("a group is a scopable model id");
}

/// The caller-dimension allowlist: what a gateway will lift out of a
/// request's `metadata` and make filterable.
#[test]
fn trace_keys_default_to_the_useful_dimensions() {
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\" }]\n",
        &[],
    )
    .expect("a config that says nothing about tracing is valid");
    for key in ["workflow_id", "chart_id", "agent", "stage", "service"] {
        assert!(
            config.usage.trace_keys.contains(key),
            "`{key}` should be a dimension by default"
        );
    }
    assert_eq!(config.usage.trace_value_chars, 128);
}

#[test]
fn trace_keys_are_canonicalised_and_may_be_narrowed() {
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\" }]\n\n\
         [usage]\ntrace_keys = [\"workflow_id\", \"event_processing_tag\", \"patient_id\"]\n",
        &[],
    )
    .expect("an explicit key list is valid");
    // The caller's spelling folds onto the canonical name, so a filter
    // written against `stage` works whichever client sent the request.
    assert!(config.usage.trace_keys.contains("stage"));
    assert!(!config.usage.trace_keys.contains("event_processing_tag"));
    // Adding a dimension is config, not a release.
    assert!(config.usage.trace_keys.contains("patient_id"));
    // And narrowing means narrowing: what was not listed is not kept.
    assert!(!config.usage.trace_keys.contains("chart_id"));
}

#[test]
fn trace_bounds_are_enforced() {
    assert_invalid(
        "[usage]\ntrace_value_chars = 2\n",
        &[],
        "usage.trace_value_chars",
        "between",
    );
    assert_invalid(
        "[usage]\ntrace_keys = [\"work flow\"]\n",
        &[],
        "usage.trace_keys[0]",
        "alphanumeric",
    );
    assert_invalid(
        "[usage]\ntrace_keys = [\"\"]\n",
        &[],
        "usage.trace_keys[0]",
        "empty",
    );
    let many = (0..40)
        .map(|i| format!("\"k{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    assert_invalid(
        &format!("[usage]\ntrace_keys = [{many}]\n"),
        &[],
        "usage.trace_keys",
        "32",
    );
}

/// Tracing is switchable off entirely, for a gateway that must not read
/// caller metadata at all.
#[test]
fn an_empty_trace_key_list_is_valid() {
    let config = load(
        "[providers.openai]\nkeys = [{ name = \"k\", value = \"sk\" }]\n\n\
         [usage]\ntrace_keys = []\n",
        &[],
    )
    .expect("an empty list turns the feature off rather than being an error");
    assert!(config.usage.trace_keys.is_empty());
}
