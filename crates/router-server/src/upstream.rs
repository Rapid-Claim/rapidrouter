//! The upstream HTTP client: one shared hyper client with per-host
//! connection pooling, HTTPS via rustls, and plain HTTP for local and
//! test providers.

use std::time::Duration;

use axum::body::Body;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use router_core::{ErrorClass, GatewayError};

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

pub struct UpstreamClient {
    inner: Client<Connector, Body>,
}

impl UpstreamClient {
    pub fn new() -> Self {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(Duration::from_secs(2)));
        http.set_nodelay(true);
        http.enforce_http(false);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);

        let inner = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build(https);
        Self { inner }
    }

    /// Send a request; `timeout` bounds the time to response *headers*
    /// (streaming bodies are governed by client disconnect, not a clock).
    pub async fn send(
        &self,
        provider: &str,
        req: http::Request<Body>,
        timeout: Duration,
    ) -> Result<http::Response<hyper::body::Incoming>, GatewayError> {
        match tokio::time::timeout(timeout, self.inner.request(req)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(GatewayError::new(
                ErrorClass::UpstreamError,
                format!("request to provider `{provider}` failed: {err}"),
            )
            .with_provider(provider)),
            Err(_) => Err(GatewayError::new(
                ErrorClass::Timeout,
                format!("provider `{provider}` did not respond within {timeout:?}"),
            )
            .with_provider(provider)),
        }
    }
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new()
    }
}
