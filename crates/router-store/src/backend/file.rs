//! A single JSON file. No coordination to provision — what you want on a
//! laptop or one box, and what most of the test suite runs against.
//!
//! Writes are read-modify-write under a re-read: the version in the file
//! is checked immediately before replacing it, and the replacement is a
//! rename over the original, so a crash mid-write leaves the previous
//! document intact rather than a truncated one. That makes it safe for
//! more than one process, which matters because a shared volume is a
//! perfectly reasonable way to run a small fleet.
//!
//! Heartbeats are empty files under `nodes/` beside the document, timed
//! by their own mtime. Nothing is read from them, so there is no parsing
//! to get wrong and no write ordering to worry about.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use std::time::{Duration, UNIX_EPOCH};

use super::{ControlPlane, ControlPlaneError, Document, NodeBeat, Snapshot, live_within, now_ms};
use crate::state::StoreState;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

pub struct FileStore {
    path: PathBuf,
    /// Serializes this process against itself. The version re-read below
    /// covers the (unsupported but survivable) two-process case.
    write: Mutex<()>,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write: Mutex::new(()),
        }
    }

    /// Heartbeats live beside the document, so processes that share the
    /// document share the fleet view automatically.
    fn nodes_dir(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("nodes")
    }

    fn read_document(&self) -> Result<(StoreState, u64), ControlPlaneError> {
        match fs::read(&self.path) {
            Ok(bytes) => Document::decode(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((StoreState::default(), 0)),
            Err(e) => Err(ControlPlaneError::unavailable("reading the store file", e)),
        }
    }
}

#[async_trait::async_trait]
impl ControlPlane for FileStore {
    fn describe(&self) -> String {
        format!("file://{}", self.path.display())
    }

    async fn load(&self) -> Result<Snapshot, ControlPlaneError> {
        let (state, version) = self.read_document()?;
        Ok(Snapshot {
            state,
            version,
            token: (version > 0).then(|| version.to_string()),
        })
    }

    async fn commit(
        &self,
        base: &Snapshot,
        next: StoreState,
    ) -> Result<Snapshot, ControlPlaneError> {
        let _guard = self.write.lock().unwrap();
        let (_, current) = self.read_document()?;
        if current != base.version {
            return Err(ControlPlaneError::Conflict {
                expected: base.version,
                actual: current,
            });
        }
        let version = current + 1;
        let bytes = Document::encode(version, &next)?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ControlPlaneError::unavailable("creating the store directory", e))?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &bytes)
            .map_err(|e| ControlPlaneError::unavailable("writing the store file", e))?;
        restrict(&tmp);
        fs::rename(&tmp, &self.path)
            .map_err(|e| ControlPlaneError::unavailable("replacing the store file", e))?;

        Ok(Snapshot {
            state: next,
            version,
            token: Some(version.to_string()),
        })
    }

    async fn heartbeat(&self, beat: &NodeBeat) -> Result<(), ControlPlaneError> {
        let dir = self.nodes_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| ControlPlaneError::unavailable("creating the heartbeat directory", e))?;
        let path = dir.join(beat_name(&beat.id, &beat.addr));
        // Rewriting the file is what updates its mtime.
        fs::write(&path, b"")
            .map_err(|e| ControlPlaneError::unavailable("writing a heartbeat", e))?;
        Ok(())
    }

    async fn peers(&self, window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        let dir = self.nodes_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // Nobody has heartbeated yet, including us.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ControlPlaneError::unavailable("listing heartbeats", e)),
        };
        let mut beats = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((id, addr)) = parse_beat_name(name) else {
                continue;
            };
            let seen_ms = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            beats.push(NodeBeat { id, addr, seen_ms });
        }
        Ok(live_within(beats, window, now_ms()))
    }

    async fn depart(&self, id: &str) -> Result<(), ControlPlaneError> {
        let dir = self.nodes_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        let prefix = format!("{id}.");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

/// `<id>.<base64url addr>` — the whole heartbeat is in the name, and the
/// mtime is the timestamp.
fn beat_name(id: &str, addr: &str) -> String {
    format!("{id}.{}", B64.encode(addr.as_bytes()))
}

fn parse_beat_name(name: &str) -> Option<(String, String)> {
    let (id, encoded) = name.split_once('.')?;
    let addr = B64
        .decode(encoded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())?;
    Some((id.to_owned(), addr))
}

/// The document holds sealed secrets and key hashes. Ciphertext, but no
/// reason for it to be world-readable.
#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}
