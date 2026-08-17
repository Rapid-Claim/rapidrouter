//! The control plane, as every node sees it.
//!
//! A node holds an in-memory copy of one small document — config text,
//! virtual keys, sealed secrets, settings — and nothing else. The
//! authoritative copy lives in a [`backend`], which is S3, DynamoDB, or a
//! local file. There is no log to replay, no peer to catch up from, and
//! no state on the node that matters if it dies: start a task, it loads
//! the document; stop it, nothing is lost.
//!
//! Reads never touch the backend. The data plane resolves keys and routes
//! against the cached copy, refreshed on a timer, so a backend outage
//! costs writes and leaves traffic alone. Writes are compare-and-swap on
//! the document version, so two consoles editing at once produce a
//! visible conflict.
//!
//! What this deliberately does not have is consensus. Ordering comes from
//! the backend's conditional write rather than a replicated log. For a
//! document that changes when a human edits it, that is the right trade:
//! it buys elastic, disposable nodes.

pub mod backend;
mod seal;
pub mod state;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

pub use backend::{BackendSpec, ControlPlane, ControlPlaneError, NodeBeat, Snapshot};
pub use seal::{KeyError, MASTER_KEY_ENV, SealedSecret, Sealer};
pub use state::{Command, StoreState};

use router_core::SecretString;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    ControlPlane(#[from] ControlPlaneError),
    #[error("secret unseal failed — is {MASTER_KEY_ENV} the same on every node?")]
    Unseal,
}

/// How many times a write without an explicit expected version will
/// re-read and retry after losing a race. Conflicts are rare (a human is
/// typing), so a small bound is enough and keeps a pathological loop from
/// hammering the backend.
const COMMIT_ATTEMPTS: usize = 5;

pub struct Store {
    plane: Arc<dyn ControlPlane>,
    sealer: Sealer,
    node_id: String,
    /// Set after the config is parsed, because the port it advertises
    /// comes from a config the store itself may hold.
    addr: arc_swap::ArcSwap<String>,
    /// The cached document. Read on every request path; replaced whole.
    cached: arc_swap::ArcSwap<Snapshot>,
    /// Serializes this node's own writes so two admin requests do not
    /// both build on the same base and guarantee one a wasted round trip.
    writing: AsyncMutex<()>,
    live: AtomicUsize,
}

impl Store {
    /// Open the control plane described by `spec`.
    ///
    /// `data_dir` is only consulted by the file backend, for the key it
    /// mints when there is no cluster-wide one to use.
    pub async fn open(spec: &BackendSpec, data_dir: &Path, addr: &str) -> Result<Self, StoreError> {
        let sealer = if spec.needs_shared_key() {
            // A shared backend means other nodes will read what we seal.
            // Refuse to start rather than write secrets nobody else can
            // open — that failure is silent and looks like a bad API key.
            Sealer::from_env()?
        } else {
            match Sealer::from_env() {
                Ok(sealer) => sealer,
                Err(KeyError::Missing) => {
                    let dir = spec.key_dir(data_dir);
                    std::fs::create_dir_all(&dir).map_err(KeyError::Io)?;
                    Sealer::load_or_create(&dir.join("node.key"))
                        .map_err(KeyError::Io)?
                        .0
                }
                Err(other) => return Err(other.into()),
            }
        };

        let plane = spec.build().await?;
        let snapshot = plane.load().await?;
        Ok(Self {
            plane,
            sealer,
            // Identity is per-process and thrown away on exit. Nothing
            // depends on a node keeping its name across restarts, which
            // is precisely what makes a node replaceable.
            node_id: uuid::Uuid::now_v7().to_string(),
            addr: arc_swap::ArcSwap::from_pointee(addr.to_owned()),
            cached: arc_swap::ArcSwap::from_pointee(snapshot),
            writing: AsyncMutex::new(()),
            live: AtomicUsize::new(1),
        })
    }

    /// A store backed by nothing, for tests.
    pub async fn ephemeral() -> Self {
        Self::open(&BackendSpec::Memory, Path::new("."), "127.0.0.1:0")
            .await
            .expect("the memory backend cannot fail to open")
    }

    /// Whether this store can hold usage objects.
    pub fn holds_blobs(&self) -> bool {
        self.plane.holds_blobs()
    }

    /// Store one usage object. Keys are relative to the store's prefix.
    pub async fn put_blob(&self, key: &str, body: Vec<u8>) -> Result<(), StoreError> {
        self.plane.put_blob(key, body).await.map_err(Into::into)
    }

    /// List usage objects under `prefix`.
    pub async fn list_blobs(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.plane.list_blobs(prefix).await.map_err(Into::into)
    }

    /// Read one usage object.
    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.plane.get_blob(key).await.map_err(Into::into)
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Update the address shown in the console's fleet list.
    pub fn set_addr(&self, addr: String) {
        self.addr.store(Arc::new(addr));
    }

    pub fn describe(&self) -> String {
        self.plane.describe()
    }

    pub fn version(&self) -> u64 {
        self.cached.load().version
    }

    /// The cached document. This is the read path — no I/O.
    pub fn read(&self) -> (StoreState, u64) {
        let snapshot = self.cached.load();
        (snapshot.state.clone(), snapshot.version)
    }

    /// Pull the document from the backend. Returns the new version when it
    /// moved, so the caller can rebuild whatever derives from it.
    pub async fn refresh(&self) -> Result<Option<u64>, ControlPlaneError> {
        let fetched = self.plane.load().await?;
        let current = self.cached.load().version;
        if fetched.version == current {
            return Ok(None);
        }
        let version = fetched.version;
        self.cached.store(Arc::new(fetched));
        Ok(Some(version))
    }

    /// Apply `command` to the document and write it back.
    ///
    /// `expect` is the caller's own compare-and-swap: `Some(v)` means "I
    /// was looking at version v", and a mismatch is reported rather than
    /// retried, because the operator's edit was composed against state
    /// they can no longer see. `None` means the caller does not care, and
    /// a lost race is simply retried against fresh state.
    pub async fn commit(
        &self,
        expect: Option<u64>,
        command: Command,
    ) -> Result<u64, ControlPlaneError> {
        let _serialize = self.writing.lock().await;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let base = self.cached.load_full();
            if let Some(expected) = expect
                && expected != base.version
            {
                return Err(ControlPlaneError::Conflict {
                    expected,
                    actual: base.version,
                });
            }

            let mut next = base.state.clone();
            next.apply(&command);

            match self.plane.commit(&base, next).await {
                Ok(snapshot) => {
                    let version = snapshot.version;
                    self.cached.store(Arc::new(snapshot));
                    return Ok(version);
                }
                Err(ControlPlaneError::Conflict { actual, .. }) => {
                    // Someone else wrote. Take their version, then either
                    // hand the conflict to the operator or try again.
                    let _ = self.refresh().await;
                    if let Some(expected) = expect {
                        return Err(ControlPlaneError::Conflict { expected, actual });
                    }
                    if attempt >= COMMIT_ATTEMPTS {
                        return Err(ControlPlaneError::Unavailable(format!(
                            "gave up after {COMMIT_ATTEMPTS} attempts: the store is being \
                             written faster than this node can keep up"
                        )));
                    }
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Announce this node and recount the fleet. One call so a caller on a
    /// timer makes two backend requests, not four.
    pub async fn beat(&self, window: Duration) -> Result<usize, ControlPlaneError> {
        let beat = NodeBeat {
            id: self.node_id.clone(),
            addr: self.addr.load().as_str().to_owned(),
            seen_ms: backend::now_ms(),
        };
        self.plane.heartbeat(&beat).await?;
        let peers = self.plane.peers(window).await?;
        // A backend with no shared view reports nobody; we are still here.
        let live = peers.len().max(1);
        self.live.store(live, Ordering::Relaxed);
        Ok(live)
    }

    /// Every node seen recently, for the console.
    pub async fn peers(&self, window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        self.plane.peers(window).await
    }

    /// The last counted fleet size, never below one. This is what rate
    /// limits divide by, so it is read on the request path and must not
    /// block.
    pub fn live_nodes(&self) -> usize {
        self.live.load(Ordering::Relaxed).max(1)
    }

    /// Remove this node's heartbeat on a clean shutdown, so its share of
    /// every rate limit returns to the fleet now rather than when the
    /// liveness window expires.
    pub async fn depart(&self) {
        if let Err(err) = self.plane.depart(&self.node_id).await {
            tracing::warn!(%err, "could not remove this node's heartbeat");
        }
    }

    /// Encrypt a secret value for storage. The plaintext never persists.
    pub fn seal_secret(&self, plaintext: &str) -> SealedSecret {
        self.sealer.seal(plaintext.as_bytes())
    }

    /// Decrypt a stored secret into the redaction-safe type.
    pub fn unseal_secret(&self, sealed: &SealedSecret) -> Result<SecretString, StoreError> {
        let bytes = self.sealer.unseal(sealed).ok_or(StoreError::Unseal)?;
        String::from_utf8(bytes)
            .map(SecretString::new)
            .map_err(|_| StoreError::Unseal)
    }

    /// Resolve a `store.<name>` reference for config validation. Returns
    /// `None` for unknown names or undecryptable values.
    pub fn resolve_secret(&self, name: &str) -> Option<String> {
        let sealed = self.cached.load().state.secrets.get(name).cloned()?;
        let bytes = self.sealer.unseal(&sealed)?;
        String::from_utf8(bytes).ok()
    }
}
