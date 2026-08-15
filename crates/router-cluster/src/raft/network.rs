//! Cluster transport: Raft RPCs and peer scatter-gather over the cluster
//! port.
//!
//! The wire format is JSON over HTTP/1.1, spoken with a small hand-rolled
//! client so the cluster port pulls in no server framework of its own.
//! Every request carries the join token: a peer that cannot present it is
//! not part of this cluster.

use std::time::Duration;

use openraft::BasicNode;
use openraft::error::{InstallSnapshotError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{NodeId, TypeConfig};
use crate::token::JoinToken;

/// The RPCs a cluster member serves on its cluster port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRpc {
    AppendEntries,
    Vote,
    InstallSnapshot,
}

impl RaftRpc {
    pub fn path(self) -> &'static str {
        match self {
            Self::AppendEntries => "/cluster/raft/append",
            Self::Vote => "/cluster/raft/vote",
            Self::InstallSnapshot => "/cluster/raft/snapshot",
        }
    }

    pub fn from_path(path: &str) -> Option<Self> {
        match path {
            "/cluster/raft/append" => Some(Self::AppendEntries),
            "/cluster/raft/vote" => Some(Self::Vote),
            "/cluster/raft/snapshot" => Some(Self::InstallSnapshot),
            _ => None,
        }
    }
}

/// Client for talking to peers: Raft RPCs, join requests, and the
/// scatter-gather endpoints the console merges.
#[derive(Clone)]
pub struct PeerClient {
    token: JoinToken,
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("peer {addr} unreachable: {source}")]
    Unreachable {
        addr: String,
        source: std::io::Error,
    },
    #[error("peer {addr} returned HTTP {status}: {body}")]
    Status {
        addr: String,
        status: u16,
        body: String,
    },
    #[error("peer {addr} sent an unreadable response: {message}")]
    Malformed { addr: String, message: String },
}

impl PeerClient {
    pub fn new(token: JoinToken) -> Self {
        Self { token }
    }

    pub fn token(&self) -> &JoinToken {
        &self.token
    }

    /// POST JSON to a peer and decode its JSON reply.
    pub async fn post<Req: Serialize, Res: DeserializeOwned>(
        &self,
        addr: &str,
        path: &str,
        body: &Req,
        timeout: Duration,
    ) -> Result<Res, PeerError> {
        let payload = serde_json::to_vec(body).map_err(|e| PeerError::Malformed {
            addr: addr.to_owned(),
            message: e.to_string(),
        })?;
        let token = self.token.encode();
        let addr_owned = addr.to_owned();
        let path_owned = path.to_owned();

        let response = tokio::time::timeout(
            timeout,
            http_post(addr_owned.clone(), path_owned, token, payload),
        )
        .await
        .map_err(|_| PeerError::Unreachable {
            addr: addr_owned.clone(),
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "peer timed out"),
        })?
        .map_err(|source| PeerError::Unreachable {
            addr: addr_owned.clone(),
            source,
        })?;

        if response.status != 200 {
            return Err(PeerError::Status {
                addr: addr_owned,
                status: response.status,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            });
        }
        serde_json::from_slice(&response.body).map_err(|e| PeerError::Malformed {
            addr: addr_owned,
            message: e.to_string(),
        })
    }

    /// Ask every peer the same question at once and keep the answers that
    /// arrive. A node that is down contributes nothing rather than
    /// failing the whole view — the console shows what it can see and
    /// says which members did not answer.
    pub async fn scatter_gather<Req, Res>(
        &self,
        addrs: &[String],
        path: &str,
        body: &Req,
        timeout: Duration,
    ) -> Vec<(String, Result<Res, PeerError>)>
    where
        Req: Serialize + Sync,
        Res: DeserializeOwned + Send + 'static,
    {
        let mut set = tokio::task::JoinSet::new();
        for addr in addrs {
            let client = self.clone();
            let addr = addr.clone();
            let path = path.to_owned();
            let payload = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
            set.spawn(async move {
                let result = client.post::<_, Res>(&addr, &path, &payload, timeout).await;
                (addr, result)
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined {
                out.push(pair);
            }
        }
        out
    }
}

struct RawResponse {
    status: u16,
    body: Vec<u8>,
}

/// A minimal HTTP/1.1 POST. The cluster port speaks to peers we
/// authenticate ourselves, on an internal network, so this stays small
/// rather than dragging in a second HTTP client stack.
async fn http_post(
    addr: String,
    path: String,
    token: String,
    payload: Vec<u8>,
) -> std::io::Result<RawResponse> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         X-Caret-Cluster-Token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator")
        })?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status line"))?;
    let body = raw[split + 4..].to_vec();

    // Chunked replies only arise if a proxy rewrites them; peers always
    // send Content-Length.
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(&body)
    } else {
        body
    };
    Ok(RawResponse { status, body })
}

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&rest[..pos]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = pos + 2;
        let end = (start + size).min(rest.len());
        out.extend_from_slice(&rest[start..end]);
        rest = &rest[(end + 2).min(rest.len())..];
    }
    out
}

impl RaftNetworkFactory<TypeConfig> for PeerClient {
    type Network = PeerConnection;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        let _ = target;
        PeerConnection {
            client: self.clone(),
            addr: node.addr.clone(),
        }
    }
}

pub struct PeerConnection {
    client: PeerClient,
    addr: String,
}

impl PeerConnection {
    /// Peer failures are `Unreachable`, which openraft treats as a
    /// transient network fault and retries — the right classification for
    /// a box that is rebooting.
    async fn call<Req, Res, E>(
        &self,
        rpc: RaftRpc,
        request: Req,
        option: RPCOption,
    ) -> Result<Res, RPCError<E>>
    where
        Req: Serialize,
        Res: DeserializeOwned,
        E: std::error::Error,
    {
        self.client
            .post::<Req, Res>(&self.addr, rpc.path(), &request, option.hard_ttl())
            .await
            .map_err(|err| match err {
                PeerError::Unreachable { source, .. } => {
                    RPCError::Unreachable(Unreachable::new(&source))
                }
                other => RPCError::Unreachable(Unreachable::new(&other)),
            })
    }
}

type RPCError<E> = openraft::error::RPCError<NodeId, BasicNode, RaftError<NodeId, E>>;

impl RaftNetwork<TypeConfig> for PeerConnection {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<openraft::error::Infallible>> {
        self.call(RaftRpc::AppendEntries, request, option).await
    }

    async fn vote(
        &mut self,
        request: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<openraft::error::Infallible>> {
        self.call(RaftRpc::Vote, request, option).await
    }

    async fn install_snapshot(
        &mut self,
        request: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RPCError<InstallSnapshotError>> {
        self.call(RaftRpc::InstallSnapshot, request, option).await
    }
}
