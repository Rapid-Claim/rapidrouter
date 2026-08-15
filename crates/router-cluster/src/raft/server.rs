//! The cluster port: Raft RPCs, the join handshake, and peer
//! scatter-gather.
//!
//! Separate from the data plane's port on purpose — cluster traffic is
//! internal, token-authenticated, and must keep working when the data
//! plane is saturated.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
use serde::{Deserialize, Serialize};

use super::{ClusterNode, NodeId, TypeConfig};
use crate::token::JoinToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub node_id: NodeId,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    pub cluster: String,
    pub leader: Option<String>,
    pub members: usize,
}

/// Everything a peer may ask this node, all of it behind the join token.
pub fn router(node: Arc<ClusterNode>) -> Router {
    Router::new()
        .route("/cluster/raft/append", post(append))
        .route("/cluster/raft/vote", post(vote))
        .route("/cluster/raft/snapshot", post(snapshot))
        .route("/cluster/commit", post(commit))
        .route("/cluster/join", post(join))
        .route("/cluster/ping", post(ping))
        .route("/cluster/fleet", post(fleet))
        .route("/cluster/usage", post(usage))
        .layer(axum::middleware::from_fn_with_state(
            node.clone(),
            authenticate,
        ))
        .with_state(node)
}

/// Every cluster RPC presents the join token. A peer that cannot is not
/// part of this cluster, and says so in terms an operator can act on.
async fn authenticate(
    State(node): State<Arc<ClusterNode>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = headers
        .get("x-caret-cluster-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match node.peers().token().verify(presented) {
        Ok(()) => next.run(request).await,
        Err(err) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn append(
    State(node): State<Arc<ClusterNode>>,
    Json(request): Json<AppendEntriesRequest<TypeConfig>>,
) -> Response {
    match node.raft.append_entries(request).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => raft_error(err),
    }
}

async fn vote(
    State(node): State<Arc<ClusterNode>>,
    Json(request): Json<VoteRequest<NodeId>>,
) -> Response {
    match node.raft.vote(request).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => raft_error(err),
    }
}

async fn snapshot(
    State(node): State<Arc<ClusterNode>>,
    Json(request): Json<InstallSnapshotRequest<TypeConfig>>,
) -> Response {
    match node.raft.install_snapshot(request).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => raft_error(err),
    }
}

/// A write forwarded by another member. Only the leader can satisfy it;
/// if leadership moved in the meantime, say so rather than guessing.
async fn commit(
    State(node): State<Arc<ClusterNode>>,
    Json(command): Json<crate::state::Command>,
) -> Response {
    match node.commit(command).await {
        Ok(version) => Json(serde_json::json!({ "version": version })).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// A node asking to join. Any member accepts the request; only the leader
/// can act on it, so a non-leader answers with where to go.
async fn join(State(node): State<Arc<ClusterNode>>, Json(request): Json<JoinRequest>) -> Response {
    match node.add_voter(request.node_id, request.addr.clone()).await {
        Ok(()) => Json(JoinResponse {
            cluster: node.peers().token().cluster_id().to_owned(),
            leader: Some(node.addr.clone()),
            members: node.live_members(),
        })
        .into_response(),
        Err(super::ClusterError::NotLeader { .. }) => {
            let leader = node.leader_addr().await;
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "not the leader",
                    "leader": leader,
                })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// The cheapest possible "are you there" — deliberately does no Raft work
/// so it answers even on a node that is busy or partitioned.
async fn ping(State(node): State<Arc<ClusterNode>>) -> Response {
    Json(serde_json::json!({ "id": node.id })).into_response()
}

async fn fleet(State(node): State<Arc<ClusterNode>>) -> Response {
    Json(node.fleet().await).into_response()
}

/// Peers answer with their local usage summary; the asking node merges
/// them into the cluster-wide view. Set by the server that owns the usage
/// pipeline.
async fn usage(State(node): State<Arc<ClusterNode>>, body: axum::body::Bytes) -> Response {
    match node.usage_responder() {
        Some(responder) => Json(responder(&body)).into_response(),
        None => Json(serde_json::json!({ "groups": [] })).into_response(),
    }
}

fn raft_error<E: std::fmt::Display>(err: E) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}

/// Serve the cluster port until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    node: Arc<ClusterNode>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(node))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Ask a live member to admit this node. Tries each seed in turn and
/// follows a leader redirect, so `--join` can name any subset of the
/// cluster.
pub async fn request_join(
    token: &JoinToken,
    seeds: &[String],
    me: JoinRequest,
) -> Result<JoinResponse, String> {
    let client = super::PeerClient::new(token.clone());
    let mut errors = Vec::new();
    let mut queue: Vec<String> = seeds.to_vec();
    let mut tried = std::collections::BTreeSet::new();

    while let Some(addr) = queue.pop() {
        if !tried.insert(addr.clone()) {
            continue;
        }
        match client
            .post::<_, JoinResponse>(&addr, "/cluster/join", &me, Duration::from_secs(10))
            .await
        {
            Ok(response) => return Ok(response),
            Err(crate::raft::PeerError::Status { body, .. }) => {
                // A non-leader tells us who the leader is; follow it.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(leader) = value["leader"].as_str()
                {
                    queue.push(leader.to_owned());
                    continue;
                }
                errors.push(format!("{addr}: {body}"));
            }
            Err(err) => errors.push(err.to_string()),
        }
    }
    Err(format!(
        "could not join via any seed ({})",
        errors.join("; ")
    ))
}
