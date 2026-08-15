//! Raft storage: the log the algorithm appends to, and the state machine
//! it applies into.
//!
//! Both live under `data-dir/raft`. The state machine persists to the same
//! `snapshot.json` shape the single-node store reads, so `caret-router
//! config export` and the other offline CLI commands work against a
//! clustered node's data directory unchanged.

// openraft's error type is large, and the traits below name it in every
// signature — boxing here would just mean unboxing at each call site the
// algorithm makes.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine, Snapshot};
use openraft::{
    Entry, EntryPayload, LogId, OptionalSend, RaftLogReader, RaftSnapshotBuilder, SnapshotMeta,
    StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};

use super::{CommandResponse, NodeId, TypeConfig};
use crate::state::StoreState;

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> std::io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(None),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Durable replace: write a temp file, fsync it, rename over the target,
/// fsync the directory. A crash leaves either the old file or the new one.
fn write_json_durable<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Log store
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct LogFile {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    last_purged: Option<LogId<NodeId>>,
    entries: Vec<Entry<TypeConfig>>,
}

struct LogInner {
    path: PathBuf,
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    last_purged: Option<LogId<NodeId>>,
    entries: BTreeMap<u64, Entry<TypeConfig>>,
}

impl LogInner {
    /// The control-plane log is small (config edits and key changes, then
    /// compacted away), so persisting it whole on each mutation is both
    /// simple and fast enough — and it makes "no holes" trivially true.
    fn persist(&self) -> Result<(), StorageError<NodeId>> {
        let file = LogFile {
            vote: self.vote,
            committed: self.committed,
            last_purged: self.last_purged,
            entries: self.entries.values().cloned().collect(),
        };
        write_json_durable(&self.path, &file).map_err(|e| StorageIOError::write_logs(&e).into())
    }
}

#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Mutex<LogInner>>,
}

impl LogStore {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("log.json");
        let file: LogFile = read_json(&path)?.unwrap_or_default();
        let entries = file
            .entries
            .into_iter()
            .map(|entry| (entry.log_id.index, entry))
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(LogInner {
                path,
                vote: file.vote,
                committed: file.committed,
                last_purged: file.last_purged,
                entries,
            })),
        })
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.entries.range(range).map(|(_, e)| e.clone()).collect())
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let inner = self.inner.lock().unwrap();
        let last = inner
            .entries
            .values()
            .next_back()
            .map(|e| e.log_id)
            .or(inner.last_purged);
        Ok(LogState {
            last_purged_log_id: inner.last_purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        inner.vote = Some(*vote);
        inner.persist()
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        inner.committed = committed;
        inner.persist()
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        {
            let mut inner = self.inner.lock().unwrap();
            for entry in entries {
                inner.entries.insert(entry.log_id.index, entry);
            }
            inner.persist()?;
        }
        // persist() fsynced before we get here, so the entries are durable.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.split_off(&log_id.index);
        inner.persist()
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        inner.last_purged = Some(log_id);
        let keep = inner.entries.split_off(&(log_id.index + 1));
        inner.entries = keep;
        inner.persist()
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// The persisted state machine. Deliberately the same shape the
/// single-node store writes, so both readers understand the file.
#[derive(Default, Serialize, Deserialize)]
struct SmFile {
    version: u64,
    state: StoreState,
    #[serde(default)]
    last_applied: Option<LogId<NodeId>>,
    #[serde(default)]
    membership: StoredMembership<NodeId, openraft::BasicNode>,
}

struct SmInner {
    path: PathBuf,
    state: StoreState,
    last_applied: Option<LogId<NodeId>>,
    membership: StoredMembership<NodeId, openraft::BasicNode>,
    snapshot: Option<Snapshot<TypeConfig>>,
}

impl SmInner {
    fn persist(&self) -> Result<(), StorageError<NodeId>> {
        let file = SmFile {
            version: self.last_applied.map(|l| l.index).unwrap_or(0),
            state: self.state.clone(),
            last_applied: self.last_applied,
            membership: self.membership.clone(),
        };
        write_json_durable(&self.path, &file)
            .map_err(|e| StorageIOError::write_state_machine(&e).into())
    }
}

pub struct StateMachineStore {
    inner: Mutex<SmInner>,
}

impl StateMachineStore {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("snapshot.json");
        let file: SmFile = read_json(&path)?.unwrap_or_default();
        Ok(Self {
            inner: Mutex::new(SmInner {
                path,
                state: file.state,
                last_applied: file.last_applied,
                membership: file.membership,
                snapshot: None,
            }),
        })
    }

    /// Local, lock-free-enough read for the data plane and console: a
    /// clone of the small control-plane document plus the log index it
    /// reflects.
    pub fn read(&self) -> (StoreState, u64) {
        let inner = self.inner.lock().unwrap();
        (
            inner.state.clone(),
            inner.last_applied.map(|l| l.index).unwrap_or(0),
        )
    }
}

impl RaftStateMachine<TypeConfig> for Arc<StateMachineStore> {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<NodeId>>,
            StoredMembership<NodeId, openraft::BasicNode>,
        ),
        StorageError<NodeId>,
    > {
        let inner = self.inner.lock().unwrap();
        Ok((inner.last_applied, inner.membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<CommandResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut inner = self.inner.lock().unwrap();
        let mut responses = Vec::new();
        for entry in entries {
            inner.last_applied = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => {}
                EntryPayload::Normal(command) => inner.state.apply(&command),
                EntryPayload::Membership(membership) => {
                    inner.membership = StoredMembership::new(Some(entry.log_id), membership);
                }
            }
            responses.push(CommandResponse {
                version: entry.log_id.index,
            });
        }
        // Persisting on apply is what lets snapshots be non-durable and
        // keeps the file readable by the offline CLI at all times.
        inner.persist()?;
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let state: StoreState = serde_json::from_slice(snapshot.get_ref()).map_err(|e| {
            StorageIOError::read_snapshot(Some(meta.signature()), openraft::AnyError::new(&e))
        })?;
        let mut inner = self.inner.lock().unwrap();
        inner.state = state;
        inner.last_applied = meta.last_log_id;
        inner.membership = meta.last_membership.clone();
        inner.persist()?;
        inner.snapshot = Some(Snapshot {
            meta: meta.clone(),
            snapshot,
        });
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.snapshot.as_ref().map(|s| Snapshot {
            meta: s.meta.clone(),
            snapshot: Box::new(Cursor::new(s.snapshot.get_ref().clone())),
        }))
    }
}

impl RaftSnapshotBuilder<TypeConfig> for Arc<StateMachineStore> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        let bytes = serde_json::to_vec(&inner.state)
            .map_err(|e| StorageIOError::read_state_machine(openraft::AnyError::new(&e)))?;
        let meta = SnapshotMeta {
            last_log_id: inner.last_applied,
            last_membership: inner.membership.clone(),
            snapshot_id: format!(
                "{}-{}",
                inner.last_applied.map(|l| l.index).unwrap_or(0),
                inner.last_applied.map(|l| l.leader_id.term).unwrap_or(0)
            ),
        };
        inner.snapshot = Some(Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(Cursor::new(bytes.clone())),
        });
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(bytes)),
        })
    }
}
