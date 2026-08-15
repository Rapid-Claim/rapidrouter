//! Well-known provider names: their adapter kind, default base URL, and the
//! conventional environment variable used by zero-config discovery.

use super::ProviderKind;

pub struct Preset {
    pub kind: ProviderKind,
    pub base_url: Option<&'static str>,
    /// Conventional env var for zero-config discovery; `None` for providers
    /// that need more than a key (Azure, Bedrock) or none at all.
    pub discovery_env: Option<&'static str>,
    /// Whether the provider is usable with no API key (local servers).
    pub keyless_ok: bool,
}

pub fn preset(name: &str) -> Option<Preset> {
    let p = match name {
        "openai" => Preset {
            kind: ProviderKind::OpenAi,
            base_url: Some("https://api.openai.com/v1"),
            discovery_env: Some("OPENAI_API_KEY"),
            keyless_ok: false,
        },
        "anthropic" => Preset {
            kind: ProviderKind::Anthropic,
            base_url: Some("https://api.anthropic.com"),
            discovery_env: Some("ANTHROPIC_API_KEY"),
            keyless_ok: false,
        },
        "gemini" => Preset {
            kind: ProviderKind::Gemini,
            base_url: Some("https://generativelanguage.googleapis.com"),
            discovery_env: Some("GEMINI_API_KEY"),
            keyless_ok: false,
        },
        "azure" => Preset {
            kind: ProviderKind::Azure,
            base_url: None,
            discovery_env: None,
            keyless_ok: false,
        },
        "bedrock" => Preset {
            kind: ProviderKind::Bedrock,
            base_url: None,
            discovery_env: None,
            keyless_ok: false,
        },
        "groq" => openai_compat("https://api.groq.com/openai/v1", Some("GROQ_API_KEY")),
        "mistral" => openai_compat("https://api.mistral.ai/v1", Some("MISTRAL_API_KEY")),
        "cerebras" => openai_compat("https://api.cerebras.ai/v1", Some("CEREBRAS_API_KEY")),
        "openrouter" => openai_compat("https://openrouter.ai/api/v1", Some("OPENROUTER_API_KEY")),
        "ollama" => Preset {
            kind: ProviderKind::OpenAiCompat,
            base_url: Some("http://localhost:11434/v1"),
            discovery_env: None,
            keyless_ok: true,
        },
        "vllm" => Preset {
            kind: ProviderKind::OpenAiCompat,
            base_url: Some("http://localhost:8000/v1"),
            discovery_env: None,
            keyless_ok: true,
        },
        _ => return None,
    };
    Some(p)
}

fn openai_compat(base_url: &'static str, discovery_env: Option<&'static str>) -> Preset {
    Preset {
        kind: ProviderKind::OpenAiCompat,
        base_url: Some(base_url),
        discovery_env,
        keyless_ok: false,
    }
}

/// Providers checked (in this order) by zero-config discovery.
pub const DISCOVERY_ORDER: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "groq",
    "mistral",
    "cerebras",
    "openrouter",
];
