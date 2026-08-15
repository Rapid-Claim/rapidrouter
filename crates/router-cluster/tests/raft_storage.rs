//! openraft's storage conformance suite, run against our log store and
//! state machine.
//!
//! Consensus correctness rests on the storage layer honoring a long list
//! of subtle contracts (no holes after truncate, purge semantics, vote
//! durability, snapshot round-trips). Rather than invent tests for those,
//! we run the ones the algorithm's authors wrote.

#![allow(clippy::result_large_err)] // openraft's error type, not ours

use std::sync::Arc;

use openraft::StorageError;
use openraft::testing::{StoreBuilder, Suite};
use router_cluster::raft::{StateMachineStore, TypeConfig};

struct TempStoreBuilder;

impl
    StoreBuilder<
        TypeConfig,
        router_cluster::raft::LogStore,
        Arc<StateMachineStore>,
        tempfile::TempDir,
    > for TempStoreBuilder
{
    async fn build(
        &self,
    ) -> Result<
        (
            tempfile::TempDir,
            router_cluster::raft::LogStore,
            Arc<StateMachineStore>,
        ),
        StorageError<u64>,
    > {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = router_cluster::raft::LogStore::open(dir.path()).expect("log store");
        let sm = Arc::new(StateMachineStore::open(dir.path()).expect("state machine"));
        Ok((dir, log, sm))
    }
}

#[test]
fn passes_openraft_storage_conformance_suite() -> Result<(), StorageError<u64>> {
    Suite::test_all(TempStoreBuilder)
}
