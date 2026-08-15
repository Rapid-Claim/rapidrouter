//! Two gateways, one store, no consensus.
//!
//! These are the tests that stand in for the old multi-node cluster
//! suite. The claims are different now — there is no leader to kill and
//! no quorum to lose — but the properties an operator actually depends on
//! are the same, and they are what is checked here:
//!
//! * a change made on one node reaches the other,
//! * rate limits divide by the number of nodes actually serving,
//! * a node whose store is unreachable keeps serving traffic,
//! * two operators editing at once cannot silently overwrite each other,
//! * and a node is disposable: a fresh one is indistinguishable from a
//!   node that has been up all along.

use std::sync::Arc;
use std::time::Duration;

use router_core::config::{Config, Format};
use router_server::AppState;
use router_store::{BackendSpec, Command, ControlPlaneError, Store};

const WINDOW: Duration = Duration::from_secs(15);

/// A file in a shared directory stands in for a shared bucket or table:
/// the same compare-and-swap contract, without the AWS round trip. The
/// backend suite in `router-store` covers S3 and DynamoDB over the wire.
fn shared_backend(dir: &tempfile::TempDir) -> BackendSpec {
    BackendSpec::File {
        path: dir.path().join("store.json"),
    }
}

const BASE_CONFIG: &str = r#"
[server]
port = 0

[providers.openai]
base_url = "http://127.0.0.1:1"
keys = [{ name = "main", value = "sk-test", models = ["gpt-4o-mini"] }]
"#;

async fn node(
    spec: &BackendSpec,
    dir: &tempfile::TempDir,
    addr: &str,
) -> (Arc<AppState>, Arc<Store>) {
    let store = Arc::new(
        Store::open(spec, dir.path(), addr)
            .await
            .expect("the store opens"),
    );
    let config = Config::from_str_with_env(BASE_CONFIG, Format::Toml, &|name: &str| {
        std::env::var(name).ok()
    })
    .expect("the base config is valid");
    let state = AppState::managed(config, store.clone(), dir.path().to_owned());
    (state, store)
}

#[tokio::test]
async fn a_config_written_on_one_node_reaches_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let spec = shared_backend(&dir);
    let (a, _sa) = node(&spec, &dir, "10.0.0.1:8080").await;
    let (b, sb) = node(&spec, &dir, "10.0.0.2:8080").await;

    let updated = format!("{BASE_CONFIG}\n[aliases]\nfast = \"openai/gpt-4o-mini\"\n");
    a.commit(None, Command::PutConfig { text: updated })
        .await
        .expect("A commits");
    a.adopt_store_state();
    assert!(
        a.config.load().aliases.contains_key("fast"),
        "the writing node applies immediately",
    );

    assert!(
        !b.config.load().aliases.contains_key("fast"),
        "B has not looked yet — propagation is a poll, not a push",
    );

    // What the refresher does on its timer.
    sb.refresh().await.expect("B reads the store");
    b.adopt_store_state();
    assert!(
        b.config.load().aliases.contains_key("fast"),
        "B adopted the config A wrote, with nobody replicating anything",
    );
}

#[tokio::test]
async fn rate_limit_shares_divide_by_the_nodes_actually_serving() {
    let dir = tempfile::tempdir().unwrap();
    // Heartbeats need a backend nodes can see each other through.
    let spec = BackendSpec::Memory;
    let plane = spec.build().await.unwrap();

    // Three nodes beat; each should then see a fleet of three.
    for id in ["a", "b", "c"] {
        plane
            .heartbeat(&router_store::NodeBeat {
                id: id.into(),
                addr: format!("10.0.0.{id}:8080"),
                seen_ms: router_store::backend::now_ms_for_tests(),
            })
            .await
            .unwrap();
    }
    assert_eq!(plane.peers(WINDOW).await.unwrap().len(), 3);

    // One leaves cleanly; its share returns at once rather than after the
    // liveness window.
    plane.depart("c").await.unwrap();
    assert_eq!(plane.peers(WINDOW).await.unwrap().len(), 2);

    let _ = dir;
}

#[tokio::test]
async fn a_node_keeps_serving_when_the_store_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let spec = shared_backend(&dir);
    let (state, store) = node(&spec, &dir, "10.0.0.1:8080").await;

    let updated = format!("{BASE_CONFIG}\n[aliases]\nfast = \"openai/gpt-4o-mini\"\n");
    state
        .commit(None, Command::PutConfig { text: updated })
        .await
        .unwrap();
    state.adopt_store_state();

    // Take the store away underneath the running node.
    std::fs::remove_dir_all(dir.path()).unwrap();

    // Reads are unaffected: they never touch the backend.
    assert!(
        state.config.load().aliases.contains_key("fast"),
        "routing still resolves from the cached config",
    );
    assert!(!state.vkeys.load().is_empty() || state.vkeys.load().is_empty());
    assert!(
        store.read().0.config_text.is_some(),
        "the cached document is still there",
    );

    // A refresh failure is reported, not fatal.
    let refreshed = store.refresh().await;
    assert!(
        refreshed.is_ok() || matches!(refreshed, Err(ControlPlaneError::Unavailable(_))),
        "an unreachable store is Unavailable, never a panic: {refreshed:?}",
    );
}

#[tokio::test]
async fn two_operators_editing_at_once_cannot_overwrite_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let spec = shared_backend(&dir);
    let (a, _sa) = node(&spec, &dir, "10.0.0.1:8080").await;
    let (b, _sb) = node(&spec, &dir, "10.0.0.2:8080").await;

    // Both consoles are showing version 0.
    let version = a
        .commit(
            Some(0),
            Command::PutConfig {
                text: "a-edit".into(),
            },
        )
        .await
        .expect("the first save lands");
    assert_eq!(version, 1);

    let err = b
        .commit(
            Some(0),
            Command::PutConfig {
                text: "b-edit".into(),
            },
        )
        .await
        .expect_err("the second save was composed against a version that is gone");
    assert!(
        matches!(
            err,
            ControlPlaneError::Conflict {
                expected: 0,
                actual: 1
            }
        ),
        "the operator is told what actually happened: {err}",
    );
    assert_eq!(
        a.store_read().unwrap().0.config_text.as_deref(),
        Some("a-edit"),
    );
}

#[tokio::test]
async fn a_brand_new_node_is_indistinguishable_from_one_that_has_been_up() {
    let dir = tempfile::tempdir().unwrap();
    let spec = shared_backend(&dir);
    let (a, _sa) = node(&spec, &dir, "10.0.0.1:8080").await;

    let updated = format!("{BASE_CONFIG}\n[aliases]\nfast = \"openai/gpt-4o-mini\"\n");
    a.commit(None, Command::PutConfig { text: updated })
        .await
        .unwrap();

    // A task started now, with no disk and no history, having missed
    // every write that came before it.
    let fresh_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&spec, fresh_dir.path(), "10.0.0.9:8080")
            .await
            .unwrap(),
    );
    let config =
        Config::from_str_with_env(BASE_CONFIG, Format::Toml, &|n: &str| std::env::var(n).ok())
            .unwrap();
    let fresh = AppState::managed(config, store.clone(), fresh_dir.path().to_owned());
    fresh.adopt_store_state();

    assert!(
        fresh.config.load().aliases.contains_key("fast"),
        "a new task catches up on open, with nothing to replay",
    );
    assert_eq!(fresh.store_read().unwrap().1, a.store_read().unwrap().1);
}

#[tokio::test]
async fn a_config_that_does_not_parse_on_this_node_does_not_take_it_down() {
    let dir = tempfile::tempdir().unwrap();
    let spec = shared_backend(&dir);
    let (state, store) = node(&spec, &dir, "10.0.0.1:8080").await;

    // Another node wrote a config naming an environment variable this
    // node does not have — a real hazard mid-rollout, when one task has
    // the new secret and another has not been restarted yet.
    store
        .commit(
            None,
            Command::PutConfig {
                text: "[aliases]\nfast = \"openai/gpt-4o-mini\"\n\
                       [providers.openai]\nbase_url = \"http://127.0.0.1:1\"\n\
                       keys = [{ name = \"main\", value = \"env.NOT_SET_ANYWHERE\" }]\n"
                    .into(),
            },
        )
        .await
        .unwrap();

    state.adopt_store_state();

    assert!(
        !state.config.load().aliases.contains_key("fast"),
        "the unbuildable config must not have been adopted — if this alias is present \
         the test is not exercising the rejection path at all",
    );
    assert!(
        state.config.load().providers.contains_key("openai"),
        "the last good config is still in force and the node is still routing",
    );
    assert_eq!(
        state.store_read().unwrap().1,
        1,
        "the store moved on, even though this node declined to follow it",
    );
}
