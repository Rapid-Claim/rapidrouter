//! An in-process backend. Used by tests and by `--ephemeral`, where the
//! control plane is expected to vanish with the process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::{ControlPlane, ControlPlaneError, NodeBeat, Snapshot, live_within, now_ms};
use crate::state::StoreState;

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    state: StoreState,
    version: u64,
    beats: HashMap<String, NodeBeat>,
}

fn token(version: u64) -> Option<String> {
    (version > 0).then(|| version.to_string())
}

#[async_trait::async_trait]
impl ControlPlane for MemoryStore {
    fn describe(&self) -> String {
        "memory".into()
    }

    async fn load(&self) -> Result<Snapshot, ControlPlaneError> {
        let inner = self.inner.lock().unwrap();
        Ok(Snapshot {
            state: inner.state.clone(),
            version: inner.version,
            token: token(inner.version),
        })
    }

    async fn commit(
        &self,
        base: &Snapshot,
        next: StoreState,
    ) -> Result<Snapshot, ControlPlaneError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.version != base.version {
            return Err(ControlPlaneError::Conflict {
                expected: base.version,
                actual: inner.version,
            });
        }
        inner.version += 1;
        inner.state = next.clone();
        Ok(Snapshot {
            state: next,
            version: inner.version,
            token: token(inner.version),
        })
    }

    async fn heartbeat(&self, beat: &NodeBeat) -> Result<(), ControlPlaneError> {
        self.inner
            .lock()
            .unwrap()
            .beats
            .insert(beat.id.clone(), beat.clone());
        Ok(())
    }

    async fn peers(&self, window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        let beats: Vec<NodeBeat> = self.inner.lock().unwrap().beats.values().cloned().collect();
        Ok(live_within(beats, window, now_ms()))
    }

    async fn depart(&self, id: &str) -> Result<(), ControlPlaneError> {
        self.inner.lock().unwrap().beats.remove(id);
        Ok(())
    }
}
