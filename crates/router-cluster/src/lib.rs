//! The embedded replicated store: control-plane state that survives
//! restarts and, in cluster mode, converges across members.
//!
//! A single node is a cluster of one — the same log-structured write path
//! (append to the WAL, apply to the state machine, bump the version) that a
//! multi-node deployment routes through consensus. Writes carry the version
//! they were based on; concurrent edits conflict visibly instead of
//! last-write-wins.
//!
//! On disk under `--data-dir`:
//!
//! ```text
//! data-dir/
//! ├── node.key            # data-encryption key + node identity (0600)
//! ├── LOCK                # advisory lock; one process per data dir
//! └── raft/
//!     ├── wal.jsonl       # command log since the last snapshot
//!     └── snapshot.json   # last compacted state
//! ```

pub mod raft;
mod seal;
pub mod state;
pub mod token;
mod wal;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use seal::SealedSecret;
pub use state::{Command, StoreState};

use router_core::SecretString;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("store is corrupt: {0}")]
    Corrupt(String),
    #[error("version conflict: expected {expected}, store is at {actual}")]
    CasConflict { expected: u64, actual: u64 },
    #[error("data dir is locked by another process (is the gateway running?)")]
    Locked,
    #[error("secret unseal failed (wrong node key?)")]
    Unseal,
    #[error(
        "this data directory is open read-only (the gateway is running); writes must go through it"
    )]
    ReadOnly,
}

struct Inner {
    state: StoreState,
    version: u64,
    wal: wal::Wal,
    commits_since_snapshot: u64,
}

/// The store handle. All mutation goes through [`Store::commit`]; reads
/// clone the (small) state.
pub struct Store {
    dir: PathBuf,
    sealer: seal::Sealer,
    node_id: String,
    inner: Mutex<Inner>,
    read_only: bool,
    _lock: Option<fs::File>,
}

/// Snapshot every N commits; config writes are rare, so compaction mostly
/// happens at shutdown.
const SNAPSHOT_EVERY: u64 = 64;

impl Store {
    /// Open the store without taking the exclusive lock, for commands
    /// that only read. This is what lets `cluster token`, `config
    /// export`, and `key ls` work against a *running* node's data dir —
    /// an operator should not have to stop the gateway to read its
    /// configuration back out.
    ///
    /// Writes through a read-only handle are refused rather than racing
    /// the running process.
    pub fn open_read_only(data_dir: &Path) -> Result<Self, StoreError> {
        Self::open_inner(data_dir, false)
    }

    /// Open (creating if needed) the store in `data_dir`: acquire the
    /// process lock, load or mint the node key, then recover state from
    /// the latest snapshot plus the WAL tail.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        Self::open_inner(data_dir, true)
    }

    fn open_inner(data_dir: &Path, exclusive: bool) -> Result<Self, StoreError> {
        fs::create_dir_all(data_dir.join("raft"))?;
        let lock = if exclusive {
            Some(acquire_lock(&data_dir.join("LOCK"))?)
        } else {
            None
        };
        let (sealer, node_id) = seal::Sealer::load_or_create(&data_dir.join("node.key"))?;

        let snapshot_path = data_dir.join("raft/snapshot.json");
        let (mut state, mut version) = match load_snapshot(&snapshot_path)? {
            Some((state, version)) => (state, version),
            None => (StoreState::default(), 0),
        };

        let wal_path = data_dir.join("raft/wal.jsonl");
        let (wal, entries) = wal::Wal::open(&wal_path)?;
        let mut replayed = 0u64;
        for entry in entries {
            if entry.index <= version {
                continue; // already folded into the snapshot
            }
            if entry.index != version + 1 {
                return Err(StoreError::Corrupt(format!(
                    "wal gap: snapshot at {version}, next entry is {}",
                    entry.index
                )));
            }
            state.apply(&entry.command);
            version = entry.index;
            replayed += 1;
        }

        Ok(Self {
            dir: data_dir.to_owned(),
            sealer,
            node_id,
            inner: Mutex::new(Inner {
                state,
                version,
                wal,
                commits_since_snapshot: replayed,
            }),
            read_only: !exclusive,
            _lock: lock,
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The Raft node id: a stable u64 derived from this node's identity,
    /// so a box keeps its place in membership across restarts.
    pub fn raft_node_id(&self) -> u64 {
        let digest = blake3::hash(self.node_id.as_bytes());
        u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("32-byte digest"))
    }

    /// The cluster's join credential, minted and persisted on first use so
    /// every node of a cluster shares one.
    pub fn join_token(&self) -> Result<crate::token::JoinToken, StoreError> {
        {
            let inner = self.inner.lock().unwrap();
            if let (Some(cluster), Some(secret)) = (
                inner.state.settings.get("cluster_id"),
                inner.state.settings.get("cluster_secret"),
            ) && let Some(token) = crate::token::JoinToken::from_parts(cluster.clone(), secret)
            {
                return Ok(token);
            }
        }
        let token = crate::token::JoinToken::generate();
        self.commit(
            None,
            Command::PutSetting {
                name: "cluster_id".into(),
                value: token.cluster_id().to_owned(),
            },
        )?;
        self.commit(
            None,
            Command::PutSetting {
                name: "cluster_secret".into(),
                value: token.secret_b64(),
            },
        )?;
        Ok(token)
    }

    /// Adopt a token an operator supplied, so a joining node authenticates
    /// as a member of *their* cluster rather than minting its own.
    pub fn adopt_join_token(&self, token: &crate::token::JoinToken) -> Result<(), StoreError> {
        self.commit(
            None,
            Command::PutSetting {
                name: "cluster_id".into(),
                value: token.cluster_id().to_owned(),
            },
        )?;
        self.commit(
            None,
            Command::PutSetting {
                name: "cluster_secret".into(),
                value: token.secret_b64(),
            },
        )?;
        Ok(())
    }

    pub fn version(&self) -> u64 {
        self.inner.lock().unwrap().version
    }

    /// A point-in-time copy of the state with its version. The state is
    /// small (config text + key/secret maps), so cloning is the read path.
    pub fn read(&self) -> (StoreState, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.state.clone(), inner.version)
    }

    /// Commit a command: CAS the version, append to the WAL (fsynced),
    /// apply. Returns the new version.
    pub fn commit(&self, expect: Option<u64>, command: Command) -> Result<u64, StoreError> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(expected) = expect
            && expected != inner.version
        {
            return Err(StoreError::CasConflict {
                expected,
                actual: inner.version,
            });
        }
        let index = inner.version + 1;
        inner.wal.append(index, &command)?;
        inner.state.apply(&command);
        inner.version = index;
        inner.commits_since_snapshot += 1;
        if inner.commits_since_snapshot >= SNAPSHOT_EVERY {
            self.compact_locked(&mut inner)?;
        }
        Ok(index)
    }

    /// Fold the WAL into a fresh snapshot and truncate it. Called
    /// periodically and at graceful shutdown.
    pub fn compact(&self) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        self.compact_locked(&mut inner)
    }

    fn compact_locked(&self, inner: &mut Inner) -> Result<(), StoreError> {
        write_snapshot(
            &self.dir.join("raft/snapshot.json"),
            &inner.state,
            inner.version,
        )?;
        inner.wal.truncate()?;
        inner.commits_since_snapshot = 0;
        Ok(())
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
        let sealed = {
            let inner = self.inner.lock().unwrap();
            inner.state.secrets.get(name).cloned()
        }?;
        let bytes = self.sealer.unseal(&sealed)?;
        String::from_utf8(bytes).ok()
    }
}

fn load_snapshot(path: &Path) -> Result<Option<(StoreState, u64)>, StoreError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    #[derive(serde::Deserialize)]
    struct Snapshot {
        version: u64,
        state: StoreState,
    }
    let snap: Snapshot = serde_json::from_slice(&bytes)
        .map_err(|e| StoreError::Corrupt(format!("snapshot: {e}")))?;
    Ok(Some((snap.state, snap.version)))
}

fn write_snapshot(path: &Path, state: &StoreState, version: u64) -> Result<(), StoreError> {
    #[derive(serde::Serialize)]
    struct Snapshot<'a> {
        version: u64,
        state: &'a StoreState,
    }
    let json = serde_json::to_vec(&Snapshot { version, state })
        .map_err(|e| StoreError::Corrupt(format!("snapshot serialize: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        io::Write::write_all(&mut file, &json)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Make the rename durable.
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_lock(path: &Path) -> Result<fs::File, StoreError> {
    use std::os::unix::io::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: flock on a valid, owned descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(StoreError::Locked);
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_lock(path: &Path) -> Result<fs::File, StoreError> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?)
}
