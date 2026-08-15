use serde_json::{Value, json};

/// The unified failure taxonomy. Every error the gateway can produce maps
/// through one of these classes to a wire shape and HTTP status, so client
/// SDK retry logic behaves exactly as it does against the provider directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    InvalidRequest,
    Authentication,
    Permission,
    NotFound,
    PayloadTooLarge,
    RateLimited,
    InsufficientQuota,
    UpstreamError,
    NoCapacity,
    Timeout,
}

impl ErrorClass {
    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Authentication => 401,
            Self::Permission => 403,
            Self::NotFound => 404,
            Self::PayloadTooLarge => 413,
            Self::RateLimited | Self::InsufficientQuota => 429,
            Self::UpstreamError => 502,
            Self::NoCapacity => 503,
            Self::Timeout => 504,
        }
    }

    /// The `error.type` value in the OpenAI-format error body.
    pub fn openai_type(self) -> &'static str {
        match self {
            Self::InvalidRequest | Self::NotFound | Self::PayloadTooLarge => {
                "invalid_request_error"
            }
            Self::Authentication => "authentication_error",
            Self::Permission => "permission_error",
            Self::RateLimited | Self::InsufficientQuota => "rate_limit_error",
            Self::UpstreamError | Self::NoCapacity | Self::Timeout => "api_error",
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Authentication => "invalid_api_key",
            Self::Permission => "permission_denied",
            Self::NotFound => "not_found",
            Self::PayloadTooLarge => "payload_too_large",
            Self::RateLimited => "rate_limited",
            Self::InsufficientQuota => "insufficient_quota",
            Self::UpstreamError => "upstream_error",
            Self::NoCapacity => "no_capacity",
            Self::Timeout => "timeout",
        }
    }
}

/// A gateway error carrying everything needed to render the inbound
/// dialect's error shape and to drive breaker/retry decisions.
///
/// Upstream detail is preserved for logs and response metadata; auth
/// material can never enter this type (secrets are `SecretString`, which
/// does not display).
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub class: ErrorClass,
    pub message: String,
    /// The request parameter at fault, when one can be named.
    pub param: Option<String>,
    /// Which provider produced the failure, when one was involved.
    pub provider: Option<String>,
    pub upstream_status: Option<u16>,
}

impl GatewayError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            param: None,
            provider: None,
            upstream_status: None,
        }
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_upstream_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    /// Render the OpenAI-format error body served on `/v1` routes.
    pub fn to_openai_body(&self) -> Value {
        let mut error = json!({
            "message": self.message,
            "type": self.class.openai_type(),
            "code": self.class.code(),
            "param": self.param,
        });
        if self.provider.is_some() || self.upstream_status.is_some() {
            error["metadata"] = json!({
                "provider": self.provider,
                "upstream_status": self.upstream_status,
            });
        }
        json!({ "error": error })
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.code(), self.message)
    }
}

impl std::error::Error for GatewayError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_match_taxonomy() {
        assert_eq!(ErrorClass::InvalidRequest.http_status(), 400);
        assert_eq!(ErrorClass::RateLimited.http_status(), 429);
        assert_eq!(ErrorClass::InsufficientQuota.http_status(), 429);
        assert_eq!(ErrorClass::Timeout.http_status(), 504);
    }

    #[test]
    fn openai_body_shape() {
        let e = GatewayError::new(ErrorClass::NotFound, "unknown model `foo`").with_param("model");
        let body = e.to_openai_body();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["param"], "model");
        assert!(body["error"].get("metadata").is_none());
    }

    #[test]
    fn upstream_detail_lands_in_metadata() {
        let e = GatewayError::new(ErrorClass::UpstreamError, "provider returned 500")
            .with_provider("openai")
            .with_upstream_status(500);
        let body = e.to_openai_body();
        assert_eq!(body["error"]["metadata"]["provider"], "openai");
        assert_eq!(body["error"]["metadata"]["upstream_status"], 500);
    }
}
