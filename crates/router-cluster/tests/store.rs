//! Store durability: WAL replay after crashes, torn-write recovery,
//! snapshot/restore, CAS semantics, sealed secrets, and the process lock.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;

use router_cluster::{Command, Store, StoreError};
use router_core::vkey::{self, VirtualKeyDef};

fn key_def(id: &str) -> VirtualKeyDef {
    VirtualKeyDef {
        id: id.into(),
        name: format!("key-{id}"),
        secret_hash: vkey::hash_secret("test-secret"),
        prev_secret: None,
        models: vec!["openai/gpt-4o-mini".into()],
        budget: None,
        rate: None,
        expires_ms: None,
        tags: BTreeMap::new(),
        enabled: true,
        created_ms: 42,
    }
}

#[test]
fn commits_survive_reopen_via_wal_replay() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .commit(
                None,
                Command::PutConfig {
                    text: "[server]\nport = 9999\n".into(),
                },
            )
            .unwrap();
        store
            .commit(
                None,
                Command::PutVirtualKey {
                    def: key_def("abc123"),
                },
            )
            .unwrap();
        assert_eq!(store.version(), 2);
        // No compact(): reopen must recover purely from the WAL.
    }
    let store = Store::open(dir.path()).unwrap();
    let (state, version) = store.read();
    assert_eq!(version, 2);
    assert_eq!(
        state.config_text.as_deref(),
        Some("[server]\nport = 9999\n")
    );
    assert_eq!(state.virtual_keys["abc123"].name, "key-abc123");
}

#[test]
fn torn_tail_is_truncated_to_last_good_entry() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        for i in 0..3 {
            store
                .commit(
                    None,
                    Command::PutSetting {
                        name: format!("k{i}"),
                        value: "v".into(),
                    },
                )
                .unwrap();
        }
    }
    // Simulate a crash mid-append: garbage half-line at the tail.
    let wal = dir.path().join("raft/wal.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(&wal).unwrap();
    file.write_all(b"{\"i\":4,\"crc\":123,\"c\":{\"op\":\"put_set")
        .unwrap();
    drop(file);

    let store = Store::open(dir.path()).unwrap();
    let (state, version) = store.read();
    assert_eq!(version, 3);
    assert_eq!(state.settings.len(), 3);

    // And the store keeps working after recovery.
    store
        .commit(
            None,
            Command::PutSetting {
                name: "k3".into(),
                value: "v".into(),
            },
        )
        .unwrap();
    assert_eq!(store.version(), 4);
}

#[test]
fn corrupt_crc_drops_that_entry_and_everything_after() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        for i in 0..3 {
            store
                .commit(
                    None,
                    Command::PutSetting {
                        name: format!("k{i}"),
                        value: "v".into(),
                    },
                )
                .unwrap();
        }
    }
    let wal = dir.path().join("raft/wal.jsonl");
    let text = fs::read_to_string(&wal).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    // Flip a byte inside the second entry's command payload.
    lines[1] = lines[1].replace("\"v\"", "\"X\"");
    fs::write(&wal, lines.join("\n") + "\n").unwrap();

    let store = Store::open(dir.path()).unwrap();
    let (state, version) = store.read();
    assert_eq!(version, 1, "entries from the corrupt one on are dropped");
    assert_eq!(state.settings.len(), 1);
}

#[test]
fn snapshot_restore_plus_wal_tail() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .commit(
                None,
                Command::PutConfig {
                    text: "a = 1\n".into(),
                },
            )
            .unwrap();
        store.compact().unwrap();
        // Post-snapshot commits land in the (now empty) WAL.
        store
            .commit(
                None,
                Command::PutVirtualKey {
                    def: key_def("abc123"),
                },
            )
            .unwrap();

        let wal = fs::read_to_string(dir.path().join("raft/wal.jsonl")).unwrap();
        assert_eq!(
            wal.lines().count(),
            1,
            "wal holds only post-snapshot entries"
        );
    }
    let store = Store::open(dir.path()).unwrap();
    let (state, version) = store.read();
    assert_eq!(version, 2);
    assert_eq!(state.config_text.as_deref(), Some("a = 1\n"));
    assert!(state.virtual_keys.contains_key("abc123"));
}

#[test]
fn cas_conflict_is_visible_never_lost_update() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let v1 = store
        .commit(
            None,
            Command::PutConfig {
                text: "a = 1\n".into(),
            },
        )
        .unwrap();

    // Two editors read version 1; the second write must conflict.
    store
        .commit(
            Some(v1),
            Command::PutConfig {
                text: "a = 2\n".into(),
            },
        )
        .unwrap();
    let err = store
        .commit(
            Some(v1),
            Command::PutConfig {
                text: "a = 3\n".into(),
            },
        )
        .unwrap_err();
    match err {
        StoreError::CasConflict { expected, actual } => {
            assert_eq!(expected, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected CasConflict, got {other:?}"),
    }
    let (state, _) = store.read();
    assert_eq!(state.config_text.as_deref(), Some("a = 2\n"));
}

#[test]
fn sealed_secrets_round_trip_and_bind_to_the_node_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let sealed = store.seal_secret("sk-super-secret");
    assert!(!sealed.ct.contains("super"));
    store
        .commit(
            None,
            Command::PutSecret {
                name: "openai_key".into(),
                sealed: sealed.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        store.resolve_secret("openai_key").as_deref(),
        Some("sk-super-secret")
    );
    assert_eq!(store.resolve_secret("missing"), None);
    assert_eq!(
        store.unseal_secret(&sealed).unwrap().expose(),
        "sk-super-secret"
    );
    drop(store);

    // Same data dir, same node key: still decrypts after reopen.
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.resolve_secret("openai_key").as_deref(),
        Some("sk-super-secret")
    );
    drop(store);

    // A different node key must fail closed, not return garbage.
    fs::remove_file(dir.path().join("node.key")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.resolve_secret("openai_key"), None);
    assert!(matches!(
        store.unseal_secret(&sealed),
        Err(StoreError::Unseal)
    ));
}

#[cfg(unix)]
#[test]
fn node_key_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let mode = fs::metadata(dir.path().join("node.key"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn second_open_of_a_live_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path()).unwrap();
    assert!(matches!(Store::open(dir.path()), Err(StoreError::Locked)));
    drop(first);
    assert!(Store::open(dir.path()).is_ok());
}

#[test]
fn snapshot_every_64_commits_truncates_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for i in 0..64 {
        store
            .commit(
                None,
                Command::PutSetting {
                    name: format!("k{i}"),
                    value: "v".into(),
                },
            )
            .unwrap();
    }
    let wal = fs::read_to_string(dir.path().join("raft/wal.jsonl")).unwrap();
    assert!(
        wal.is_empty(),
        "auto-compaction folded the wal into a snapshot"
    );
    assert_eq!(store.version(), 64);
    drop(store);

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.version(), 64);
    assert_eq!(store.read().0.settings.len(), 64);
}
