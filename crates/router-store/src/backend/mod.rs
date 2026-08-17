//! The control-plane adapter: where cluster state actually lives.
//!
//! Every node is stateless. It holds a cached copy of the control-plane
//! document in memory and nothing on disk it cannot lose, because the
//! authoritative copy is in a backend that already knows how to replicate
//! — S3, DynamoDB, or a local file for a single box.
//!
//! Two operations carry the whole design:
//!
//! * [`ControlPlane::load`] returns the document and the token needed to
//!   write over it.
//! * [`ControlPlane::commit`] writes conditionally on that token, so two
//!   nodes editing at once produce a visible [`ControlPlaneError::Conflict`]
//!   rather than a silent last-write-wins.
//!
//! That is the entire consistency story: compare-and-swap on one small
//! document. It is weaker than consensus — a write is ordered by the
//! backend, not by a replicated log — and it is enough, because the
//! document is read-mostly and the data plane never reads it directly.
//! Nodes serve traffic from an in-memory snapshot refreshed on a timer, so
//! a backend outage costs you writes, not requests.
//!
//! Liveness is the other half. Rate limits divide by the number of live
//! nodes, so each node writes a heartbeat and counts the recent ones. That
//! replaces the peer-to-peer probing a clustered design would do, and it
//! means the fleet has no membership to manage: a node that stops
//! heartbeating leaves, and a node that starts joins.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::StoreState;

pub mod dynamodb;
pub mod file;
pub mod memory;
pub mod s3;

/// The control-plane document plus what is needed to write over it.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub state: StoreState,
    /// Monotonic, operator-visible, and the value the admin API does CAS
    /// against. Distinct from `token`, which is the backend's own idea of
    /// "the version I read" and may be an ETag or a sequence number.
    pub version: u64,
    /// Opaque backend concurrency token. `None` means "the document does
    /// not exist yet", which is a create rather than an overwrite.
    pub token: Option<String>,
}

impl Snapshot {
    /// The empty document a fresh deployment starts from.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// One node's heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBeat {
    pub id: String,
    /// Advertised address, for the console. Never dialed by anything.
    pub addr: String,
    /// Epoch milliseconds the backend last saw this node.
    pub seen_ms: u64,
}

impl NodeBeat {
    pub fn age(&self, now_ms: u64) -> Duration {
        Duration::from_millis(now_ms.saturating_sub(self.seen_ms))
    }
}

/// What went wrong, at the granularity an operator needs.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    /// Someone else wrote first. Re-read and retry, or tell the operator.
    #[error("version conflict: expected {expected}, store is at {actual}")]
    Conflict { expected: u64, actual: u64 },
    /// The backend is unreachable, throttled, or refusing. Traffic keeps
    /// flowing from cache; writes are refused until it returns.
    #[error("control-plane store is unavailable: {0}")]
    Unavailable(String),
    /// Misconfiguration or corruption — a human has to fix it.
    #[error("control-plane store fault: {0}")]
    Fault(String),
    /// The backend does not offer this capability.
    #[error("{0}")]
    Unsupported(String),
}

impl ControlPlaneError {
    pub fn unavailable(context: &str, err: impl std::fmt::Display) -> Self {
        Self::Unavailable(format!("{context}: {err}"))
    }
    pub fn fault(context: &str, err: impl std::fmt::Display) -> Self {
        Self::Fault(format!("{context}: {err}"))
    }
}

/// A backend. Implementations must make [`commit`](ControlPlane::commit)
/// atomic against concurrent writers — that is the one contract the rest
/// of the system leans on.
#[async_trait::async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    /// Short operator-facing description, e.g. `dynamodb://rapid-router`.
    fn describe(&self) -> String;

    /// Read the current document. A backend with nothing stored yet
    /// returns [`Snapshot::empty`] rather than an error, so first boot is
    /// not a special case anywhere above this trait.
    async fn load(&self) -> Result<Snapshot, ControlPlaneError>;

    /// Write `next` if and only if the document still matches `base`.
    async fn commit(
        &self,
        base: &Snapshot,
        next: StoreState,
    ) -> Result<Snapshot, ControlPlaneError>;

    /// Record that this node is alive. Backends without a shared view of
    /// the fleet (a local file) may no-op.
    async fn heartbeat(&self, _beat: &NodeBeat) -> Result<(), ControlPlaneError> {
        Ok(())
    }

    /// Nodes seen within `window`. The default is a fleet of one, which is
    /// the truthful answer for a backend only this process can reach.
    async fn peers(&self, _window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        Ok(Vec::new())
    }

    /// Best-effort removal of this node's heartbeat at shutdown, so a
    /// clean stop frees its rate-limit share immediately instead of after
    /// the liveness window.
    async fn depart(&self, _id: &str) -> Result<(), ControlPlaneError> {
        Ok(())
    }

    /// Store an opaque object beside the control-plane document.
    ///
    /// Usage history is shipped through here rather than through a second
    /// client: the backend already holds credentials, endpoint and TLS
    /// configuration that an operator got working once, and a separate
    /// path would be a separate thing to configure and a separate thing
    /// to break. Backends with nowhere to put objects say so, and the
    /// caller keeps history local.
    async fn put_blob(&self, _key: &str, _body: Vec<u8>) -> Result<(), ControlPlaneError> {
        Err(ControlPlaneError::Unsupported(
            "this store cannot hold usage objects".into(),
        ))
    }

    /// List object keys under `prefix`.
    async fn list_blobs(&self, _prefix: &str) -> Result<Vec<String>, ControlPlaneError> {
        Ok(Vec::new())
    }

    /// Read one object back.
    async fn get_blob(&self, _key: &str) -> Result<Option<Vec<u8>>, ControlPlaneError> {
        Ok(None)
    }

    /// Whether this backend can hold usage objects at all.
    fn holds_blobs(&self) -> bool {
        false
    }
}

/// Which backend to build, and what it needs. Parsed from `[store]` in
/// the config or the matching CLI flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpec {
    /// A file under the data directory. Single node, no coordination.
    File { path: std::path::PathBuf },
    /// One object in a bucket, written with conditional PUTs.
    S3 {
        bucket: String,
        prefix: String,
        region: Option<String>,
        endpoint: Option<String>,
    },
    /// One item in a table, written with a condition expression.
    DynamoDb {
        table: String,
        region: Option<String>,
        endpoint: Option<String>,
    },
    /// Nothing persists. Tests, and `--ephemeral`.
    Memory,
}

impl BackendSpec {
    /// The name an operator typed, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::S3 { .. } => "s3",
            Self::DynamoDb { .. } => "dynamodb",
            Self::Memory => "memory",
        }
    }

    /// Whether a node must be given a fleet-wide master key rather than
    /// minting its own.
    ///
    /// True for the network backends, where the nodes that will read a
    /// sealed secret are elsewhere. False for a file, where the fallback
    /// key sits next to the document — so processes sharing the document
    /// share the key, and a single box needs no setup at all.
    pub fn needs_shared_key(&self) -> bool {
        matches!(self, Self::S3 { .. } | Self::DynamoDb { .. })
    }

    /// Where the fallback key lives when no master key is supplied.
    /// Beside the document, so it follows the data rather than the node.
    pub fn key_dir(&self, data_dir: &std::path::Path) -> std::path::PathBuf {
        match self {
            Self::File { path } => path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| data_dir.to_path_buf()),
            _ => data_dir.to_path_buf(),
        }
    }

    pub async fn build(&self) -> Result<Arc<dyn ControlPlane>, ControlPlaneError> {
        Ok(match self {
            Self::Memory => Arc::new(memory::MemoryStore::default()),
            Self::File { path } => Arc::new(file::FileStore::new(path.clone())),
            Self::S3 {
                bucket,
                prefix,
                region,
                endpoint,
            } => Arc::new(
                s3::S3Store::new(
                    bucket.clone(),
                    prefix.clone(),
                    region.clone(),
                    endpoint.clone(),
                )
                .await?,
            ),
            Self::DynamoDb {
                table,
                region,
                endpoint,
            } => Arc::new(
                dynamodb::DynamoStore::new(table.clone(), region.clone(), endpoint.clone()).await?,
            ),
        })
    }
}

/// The wire/disk form of the document. Versioned so a future backend or
/// format change has somewhere to branch.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Document {
    pub format: u32,
    pub version: u64,
    pub state: StoreState,
}

pub(crate) const FORMAT: u32 = 1;

impl Document {
    pub fn encode(version: u64, state: &StoreState) -> Result<Vec<u8>, ControlPlaneError> {
        serde_json::to_vec(&Document {
            format: FORMAT,
            version,
            state: state.clone(),
        })
        .map_err(|e| ControlPlaneError::fault("encoding the control-plane document", e))
    }

    pub fn decode(bytes: &[u8]) -> Result<(StoreState, u64), ControlPlaneError> {
        let doc: Document = serde_json::from_slice(bytes)
            .map_err(|e| ControlPlaneError::fault("parsing the control-plane document", e))?;
        if doc.format != FORMAT {
            return Err(ControlPlaneError::Fault(format!(
                "control-plane document is format {}, this build understands {FORMAT}",
                doc.format
            )));
        }
        Ok((doc.state, doc.version))
    }
}

/// Exposed for the backend conformance suite, which needs to write
/// heartbeats with timestamps the backends will agree with.
pub fn now_ms_for_tests() -> u64 {
    now_ms()
}

/// The HTTPS client both AWS backends use.
///
/// Chosen explicitly rather than left to the SDK's default, because the
/// default links a second rustls crypto provider alongside the one the
/// gateway's own HTTP client already uses. Two registered providers make
/// `rustls` refuse to pick one and every TLS handshake in the process
/// panics — including the ones proxying traffic, which have nothing to do
/// with AWS. One provider, named here, for the whole binary.
pub(crate) fn https_client() -> aws_smithy_runtime_api::client::http::SharedHttpClient {
    use aws_smithy_http_client::{Builder, tls};
    Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https()
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drop heartbeats older than the window and collapse duplicates, keeping
/// the most recent sighting of each node.
pub(crate) fn live_within(beats: Vec<NodeBeat>, window: Duration, now_ms: u64) -> Vec<NodeBeat> {
    let mut by_id: BTreeMap<String, NodeBeat> = BTreeMap::new();
    for beat in beats {
        if beat.age(now_ms) > window {
            continue;
        }
        by_id
            .entry(beat.id.clone())
            .and_modify(|kept| {
                if beat.seen_ms > kept.seen_ms {
                    *kept = beat.clone();
                }
            })
            .or_insert(beat);
    }
    by_id.into_values().collect()
}
