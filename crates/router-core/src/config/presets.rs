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

/// The API shape a model is called with.
///
/// A provider's default dialect is decided by its [`ProviderKind`], but a
/// few models are only reachable on one endpoint of a provider that
/// serves both — OpenAI's reasoning models are Responses-only — so the
/// shape belongs on the model, not just the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFormat {
    ChatCompletions,
    Responses,
    Messages,
    GenerateContent,
    Embeddings,
}

impl ModelFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::GenerateContent => "generate_content",
            Self::Embeddings => "embeddings",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "chat_completions" => Self::ChatCompletions,
            "responses" => Self::Responses,
            "messages" => Self::Messages,
            "generate_content" => Self::GenerateContent,
            "embeddings" => Self::Embeddings,
            _ => return None,
        })
    }

    /// What a provider of this kind speaks unless a model says otherwise.
    pub fn default_for(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Anthropic | ProviderKind::ClaudeSubscription => Self::Messages,
            ProviderKind::Gemini | ProviderKind::Vertex => Self::GenerateContent,
            ProviderKind::CodexSubscription => Self::Responses,
            _ => Self::ChatCompletions,
        }
    }
}

/// A model a provider is known to serve.
pub struct CatalogModel {
    pub id: &'static str,
    pub format: ModelFormat,
}

const fn chat(id: &'static str) -> CatalogModel {
    CatalogModel {
        id,
        format: ModelFormat::ChatCompletions,
    }
}
const fn responses(id: &'static str) -> CatalogModel {
    CatalogModel {
        id,
        format: ModelFormat::Responses,
    }
}
const fn messages(id: &'static str) -> CatalogModel {
    CatalogModel {
        id,
        format: ModelFormat::Messages,
    }
}
const fn generate(id: &'static str) -> CatalogModel {
    CatalogModel {
        id,
        format: ModelFormat::GenerateContent,
    }
}

/// The models a provider is seeded with when it is added from the
/// console.
///
/// Deliberately a starting point, not an authority: model line-ups change
/// weekly and a gateway that only routes what it has heard of is a
/// gateway that blocks you on the day a new model ships. Nothing here is
/// enforced — an id absent from this list still routes — so the cost of
/// the list being stale is a missing row in a picker, not a failed
/// request.
const OPENAI_MODELS: &[CatalogModel] = &[
    chat("gpt-4o"),
    chat("gpt-4o-mini"),
    chat("gpt-4.1"),
    chat("gpt-4.1-mini"),
    chat("gpt-4.1-nano"),
    responses("o3"),
    responses("o4-mini"),
    responses("gpt-5.5"),
];
const ANTHROPIC_MODELS: &[CatalogModel] = &[
    messages("claude-opus-4-5"),
    messages("claude-sonnet-4-5"),
    messages("claude-haiku-4-5"),
];
const CODEX_MODELS: &[CatalogModel] = &[responses("gpt-5.5"), responses("gpt-5.5-codex")];
const GEMINI_MODELS: &[CatalogModel] = &[
    generate("gemini-flash-latest"),
    generate("gemini-pro-latest"),
    generate("gemini-2.5-flash"),
];
const GROQ_MODELS: &[CatalogModel] = &[
    chat("llama-3.3-70b-versatile"),
    chat("llama-3.1-8b-instant"),
];
const MISTRAL_MODELS: &[CatalogModel] =
    &[chat("mistral-large-latest"), chat("mistral-small-latest")];
const CEREBRAS_MODELS: &[CatalogModel] = &[chat("llama-3.3-70b")];
const OPENROUTER_MODELS: &[CatalogModel] =
    &[chat("openai/gpt-4o"), chat("anthropic/claude-sonnet-4-5")];

pub fn catalog(name: &str) -> &'static [CatalogModel] {
    match name {
        "openai" => OPENAI_MODELS,
        "anthropic" | "claude_subscription" => ANTHROPIC_MODELS,
        "codex_subscription" => CODEX_MODELS,
        "gemini" | "vertex" => GEMINI_MODELS,
        "groq" => GROQ_MODELS,
        "mistral" => MISTRAL_MODELS,
        "cerebras" => CEREBRAS_MODELS,
        "openrouter" => OPENROUTER_MODELS,
        _ => &[],
    }
}

/// Every provider the console offers in its "add provider" picker.
pub const ALL_PRESETS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "azure",
    "bedrock",
    "vertex",
    "databricks",
    "groq",
    "mistral",
    "cerebras",
    "openrouter",
    "ollama",
    "vllm",
];

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
        "vertex" => Preset {
            kind: ProviderKind::Vertex,
            base_url: None,
            discovery_env: None,
            keyless_ok: false,
        },
        // Databricks Foundation Model APIs are OpenAI-compatible under
        // {workspace}/serving-endpoints; the workspace URL comes from
        // config (or DATABRICKS_HOST discovery), so no preset base_url.
        "databricks" => Preset {
            kind: ProviderKind::OpenAiCompat,
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
