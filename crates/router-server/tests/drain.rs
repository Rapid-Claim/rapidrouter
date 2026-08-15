//! Graceful drain: a request whose handler is still running when shutdown
//! is requested must complete, and the server future must then resolve.

use std::time::Duration;

use axum::routing::get;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn inflight_request_completes_during_drain() {
    let config = Config::from_str_with_env(
        "[server]\ndrain_timeout_secs = 5\n\n[providers.ollama]\nauth = \"none\"\n",
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    let state = AppState::new(config);

    let app = build_router(state.clone()).route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(router_server::serve(listener, state, app, async move {
        let _ = shutdown_rx.await;
    }));

    // Get the slow request fully sent and its handler running...
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(b"GET /slow HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ...then request shutdown while it is in flight.
    shutdown_tx.send(()).unwrap();

    let mut response = String::new();
    conn.read_to_string(&mut response).await.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "in-flight request was dropped: {response:?}"
    );
    assert!(
        response.ends_with("done"),
        "truncated response: {response:?}"
    );

    // And the server future must resolve promptly once drained.
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server did not shut down after drain")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn new_connections_refused_after_drain_completes() {
    let config = Config::from_str_with_env("", Format::Toml, &|_: &str| None).unwrap();
    let state = AppState::new(config);
    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(router_server::serve(listener, state, app, async move {
        let _ = shutdown_rx.await;
    }));

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();

    // The listener is gone; new connections must fail.
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
