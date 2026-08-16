//! The store facade: cached reads, compare-and-swap writes, fleet
//! liveness, and the one property that makes stateless nodes possible —
//! a secret sealed on one node can be read on every other.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use router_core::vkey::{self, VirtualKeyDef};
use router_store::{BackendSpec, Command, ControlPlaneError, KeyError, Sealer, Store, StoreError};
use support::s3_mock::S3Mock;

const WINDOW: Duration = Duration::from_secs(15);

/// `RAPID_MASTER_KEY` is process-global, so the tests that care about it
/// take turns. An async mutex, because the guard is held across awaits.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct MasterKey;

impl MasterKey {
    fn set(value: &str) -> Self {
        unsafe { std::env::set_var(router_store::MASTER_KEY_ENV, value) };
        Self
    }
    fn unset() -> Self {
        unsafe { std::env::remove_var(router_store::MASTER_KEY_ENV) };
        Self
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(router_store::MASTER_KEY_ENV) };
    }
}

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

async fn file_store(dir: &Path) -> Store {
    Store::open(
        &BackendSpec::File {
            path: dir.join("store.json"),
        },
        dir,
        "127.0.0.1:9443",
    )
    .await
    .unwrap()
}

// ------------------------------------------------------------ basics

#[tokio::test]
async fn commits_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = file_store(dir.path()).await;
        store
            .commit(
                None,
                Command::PutConfig {
                    text: "[server]\nport = 9999\n".into(),
                },
            )
            .await
            .unwrap();
        store
            .commit(
                None,
                Command::PutVirtualKey {
                    def: key_def("vk_a"),
                },
            )
            .await
            .unwrap();
    }

    let reopened = file_store(dir.path()).await;
    let (state, version) = reopened.read();
    assert_eq!(version, 2);
    assert_eq!(
        state.config_text.as_deref(),
        Some("[server]\nport = 9999\n")
    );
    assert!(state.virtual_keys.contains_key("vk_a"));
}

#[tokio::test]
async fn an_operators_stale_edit_conflicts_rather_than_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;

    let v1 = store
        .commit(
            None,
            Command::PutConfig {
                text: "first".into(),
            },
        )
        .await
        .unwrap();
    store
        .commit(
            None,
            Command::PutConfig {
                text: "second".into(),
            },
        )
        .await
        .unwrap();

    // The console was showing v1 when the operator hit save.
    let err = store
        .commit(
            Some(v1),
            Command::PutConfig {
                text: "stale".into(),
            },
        )
        .await
        .expect_err("an edit composed against v1 must not overwrite v2");
    assert!(matches!(
        err,
        ControlPlaneError::Conflict {
            expected: 1,
            actual: 2
        }
    ));
    assert_eq!(store.read().0.config_text.as_deref(), Some("second"));
}

#[tokio::test]
async fn a_write_that_does_not_care_about_the_version_rebases_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let a = file_store(dir.path()).await;
    let b = file_store(dir.path()).await;

    // Both nodes are looking at version 0. B writes first.
    b.commit(
        None,
        Command::PutConfig {
            text: "from-b".into(),
        },
    )
    .await
    .unwrap();

    // A's cache is stale, but this write expresses no expectation, so it
    // should re-read and land on top rather than fail.
    let version = a
        .commit(
            None,
            Command::PutVirtualKey {
                def: key_def("vk_a"),
            },
        )
        .await
        .expect("a versionless write retries against fresh state");
    assert_eq!(version, 2);

    let (state, _) = a.read();
    assert_eq!(
        state.config_text.as_deref(),
        Some("from-b"),
        "B's write survived A's retry",
    );
    assert!(state.virtual_keys.contains_key("vk_a"));
}

#[tokio::test]
async fn reads_are_served_from_cache_until_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let a = file_store(dir.path()).await;
    let b = file_store(dir.path()).await;

    b.commit(
        None,
        Command::PutConfig {
            text: "b-wrote-this".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        a.read().1,
        0,
        "A has not looked at the backend since opening"
    );
    assert_eq!(
        a.refresh().await.unwrap(),
        Some(1),
        "refresh finds B's write"
    );
    assert_eq!(a.read().0.config_text.as_deref(), Some("b-wrote-this"));
    assert_eq!(
        a.refresh().await.unwrap(),
        None,
        "a refresh that finds nothing new reports nothing new",
    );
}

// ------------------------------------------------- secrets across nodes

#[tokio::test]
async fn a_secret_sealed_on_one_node_is_readable_on_every_other() {
    let _guard = ENV.lock().await;
    let (_mock, endpoint) = S3Mock::spawn().await;
    support::fake_aws_env();
    let _key = MasterKey::set(&Sealer::generate_master_key());

    let spec = BackendSpec::S3 {
        bucket: "rapid-test".into(),
        prefix: "rapid/".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(endpoint),
    };
    let dir = tempfile::tempdir().unwrap();

    let a = Store::open(&spec, dir.path(), "10.0.0.1:9443")
        .await
        .unwrap();
    let b = Store::open(&spec, dir.path(), "10.0.0.2:9443")
        .await
        .unwrap();
    assert_ne!(a.node_id(), b.node_id(), "these are two different nodes");

    a.commit(
        None,
        Command::PutSecret {
            name: "openai".into(),
            sealed: a.seal_secret("sk-live-abc123"),
        },
    )
    .await
    .unwrap();

    b.refresh().await.unwrap();
    assert_eq!(
        b.resolve_secret("openai").as_deref(),
        Some("sk-live-abc123"),
        "the whole point of a shared master key",
    );
}

#[tokio::test]
async fn a_shared_backend_refuses_to_start_without_a_master_key() {
    let _guard = ENV.lock().await;
    let (_mock, endpoint) = S3Mock::spawn().await;
    support::fake_aws_env();
    let _key = MasterKey::unset();

    let dir = tempfile::tempdir().unwrap();
    let err = Store::open(
        &BackendSpec::S3 {
            bucket: "rapid-test".into(),
            prefix: "rapid/".into(),
            region: Some("us-east-1".into()),
            endpoint: Some(endpoint),
        },
        dir.path(),
        "10.0.0.1:9443",
    )
    .await;

    match err
        .err()
        .expect("starting without a shared key would seal secrets nobody else can read")
    {
        StoreError::Key(KeyError::Missing) => {}
        other => panic!("expected a missing-key error, got {other}"),
    }
}

#[tokio::test]
async fn a_single_node_still_works_with_no_master_key_at_all() {
    let _guard = ENV.lock().await;
    let _key = MasterKey::unset();
    let dir = tempfile::tempdir().unwrap();

    let store = file_store(dir.path()).await;
    store
        .commit(
            None,
            Command::PutSecret {
                name: "openai".into(),
                sealed: store.seal_secret("sk-local"),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.resolve_secret("openai").as_deref(), Some("sk-local"));
    assert!(
        dir.path().join("node.key").exists(),
        "the single-node fallback mints a key beside the data",
    );
}

#[tokio::test]
async fn a_malformed_master_key_is_rejected_at_startup() {
    assert!(matches!(
        Sealer::from_master_key("not-base64!!"),
        Err(KeyError::Malformed)
    ));
    assert!(
        matches!(
            Sealer::from_master_key("c2hvcnQ="),
            Err(KeyError::Malformed)
        ),
        "a key of the wrong length is as bad as no key",
    );
    assert!(Sealer::from_master_key(&Sealer::generate_master_key()).is_ok());
}

#[tokio::test]
async fn secrets_do_not_decrypt_under_a_different_master_key() {
    let a = Sealer::from_master_key(&Sealer::generate_master_key()).unwrap();
    let b = Sealer::from_master_key(&Sealer::generate_master_key()).unwrap();
    let sealed = a.seal(b"sk-live");
    assert_eq!(a.unseal(&sealed).as_deref(), Some(&b"sk-live"[..]));
    assert!(
        b.unseal(&sealed).is_none(),
        "a wrong key must fail closed, not return garbage",
    );
}

// ------------------------------------------------------------- fleet

#[tokio::test]
async fn the_fleet_count_follows_heartbeats() {
    let (_mock, endpoint) = S3Mock::spawn().await;
    support::fake_aws_env();
    let _guard = ENV.lock().await;
    let _key = MasterKey::set(&Sealer::generate_master_key());

    let spec = BackendSpec::S3 {
        bucket: "rapid-test".into(),
        prefix: "rapid/".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(endpoint),
    };
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open(&spec, dir.path(), "10.0.0.1:9443")
        .await
        .unwrap();
    let b = Store::open(&spec, dir.path(), "10.0.0.2:9443")
        .await
        .unwrap();

    assert_eq!(a.beat(WINDOW).await.unwrap(), 1, "alone so far");
    assert_eq!(b.beat(WINDOW).await.unwrap(), 2, "B sees both");
    assert_eq!(a.beat(WINDOW).await.unwrap(), 2, "and so does A");

    b.depart().await;
    assert_eq!(
        a.beat(WINDOW).await.unwrap(),
        1,
        "a clean shutdown returns B's share immediately",
    );
}

#[tokio::test]
async fn a_node_with_no_shared_backend_counts_itself_and_stops_there() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;
    assert_eq!(store.live_nodes(), 1);
    assert_eq!(store.beat(WINDOW).await.unwrap(), 1);
}

#[tokio::test]
async fn the_store_file_is_not_world_readable() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;
    store
        .commit(None, Command::PutConfig { text: "x".into() })
        .await
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("store.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the document holds sealed secrets");
    }
}
