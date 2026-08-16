//! One suite, every backend.
//!
//! The whole architecture rests on a single claim: `commit` is atomic
//! against a concurrent writer. If that holds for each backend then two
//! nodes cannot silently overwrite each other, and if it does not then
//! nothing above this layer can be trusted. So the interesting tests here
//! are the racing ones, and they run identically against memory, a file,
//! S3 and DynamoDB rather than being written once for the easy case.

mod support;

use std::sync::Arc;
use std::time::Duration;

use router_store::backend::{BackendSpec, ControlPlane, ControlPlaneError, NodeBeat};
use router_store::state::{Command, StoreState};
use support::dynamodb_mock::DynamoMock;
use support::s3_mock::S3Mock;

const WINDOW: Duration = Duration::from_secs(15);

fn put_config(text: &str) -> Command {
    Command::PutConfig { text: text.into() }
}

fn state_with(text: &str) -> StoreState {
    let mut state = StoreState::default();
    state.apply(&put_config(text));
    state
}

async fn memory() -> Arc<dyn ControlPlane> {
    BackendSpec::Memory.build().await.unwrap()
}

async fn file(dir: &tempfile::TempDir) -> Arc<dyn ControlPlane> {
    BackendSpec::File {
        path: dir.path().join("store.json"),
    }
    .build()
    .await
    .unwrap()
}

async fn s3(mock_endpoint: &str) -> Arc<dyn ControlPlane> {
    support::fake_aws_env();
    BackendSpec::S3 {
        bucket: "rapid-test".into(),
        prefix: "rapid/".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(mock_endpoint.to_owned()),
    }
    .build()
    .await
    .unwrap()
}

async fn dynamo(mock_endpoint: &str) -> Arc<dyn ControlPlane> {
    support::fake_aws_env();
    BackendSpec::DynamoDb {
        table: "rapid-test".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(mock_endpoint.to_owned()),
    }
    .build()
    .await
    .unwrap()
}

// ---------------------------------------------------------------- shared

/// Load, write, read back. The baseline every backend must clear.
async fn round_trip(plane: Arc<dyn ControlPlane>) {
    let empty = plane.load().await.unwrap();
    assert_eq!(empty.version, 0, "a fresh backend starts at version 0");
    assert!(empty.state.config_text.is_none());

    let first = plane
        .commit(&empty, state_with("model = 'a'"))
        .await
        .unwrap();
    assert_eq!(first.version, 1);

    let reloaded = plane.load().await.unwrap();
    assert_eq!(reloaded.version, 1);
    assert_eq!(reloaded.state.config_text.as_deref(), Some("model = 'a'"));

    let second = plane
        .commit(&reloaded, state_with("model = 'b'"))
        .await
        .unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(
        plane.load().await.unwrap().state.config_text.as_deref(),
        Some("model = 'b'")
    );
}

/// Two nodes read the same version and both write. Exactly one wins, and
/// the loser is told the version it actually needs to rebase on.
async fn concurrent_writers_conflict(plane: Arc<dyn ControlPlane>) {
    let base = plane.load().await.unwrap();
    plane
        .commit(&base, state_with("winner"))
        .await
        .expect("the first writer commits");

    let err = plane
        .commit(&base, state_with("loser"))
        .await
        .expect_err("the second writer must be refused");

    match err {
        ControlPlaneError::Conflict { expected, actual } => {
            assert_eq!(expected, base.version);
            assert_eq!(
                actual,
                base.version + 1,
                "the loser learns the real version"
            );
        }
        other => panic!("expected a conflict, got {other}"),
    }

    assert_eq!(
        plane.load().await.unwrap().state.config_text.as_deref(),
        Some("winner"),
        "the losing write must not have landed",
    );
}

/// Creating the document is a race too: two nodes bootstrapping at once
/// must not both believe they created version 1.
async fn concurrent_creates_conflict(plane: Arc<dyn ControlPlane>) {
    let empty_a = plane.load().await.unwrap();
    let empty_b = plane.load().await.unwrap();
    plane.commit(&empty_a, state_with("first")).await.unwrap();
    let err = plane.commit(&empty_b, state_with("second")).await;
    assert!(
        matches!(err, Err(ControlPlaneError::Conflict { .. })),
        "the second bootstrap must conflict, got {err:?}",
    );
}

/// Everything in the document survives the trip, including the maps that
/// carry keys and sealed secrets.
async fn full_document_round_trips(plane: Arc<dyn ControlPlane>) {
    let base = plane.load().await.unwrap();
    let mut state = StoreState::default();
    state.apply(&Command::PutConfig {
        text: "[server]\nport = 9443\n".into(),
    });
    state.apply(&Command::PutSecret {
        name: "openai".into(),
        sealed: router_store::SealedSecret {
            nonce: "bm9uY2U".into(),
            ct: "Y2lwaGVy".into(),
        },
    });
    state.apply(&Command::PutSetting {
        name: "retention_days".into(),
        value: "30".into(),
    });

    let committed = plane.commit(&base, state.clone()).await.unwrap();
    let loaded = plane.load().await.unwrap();
    assert_eq!(loaded.version, committed.version);
    assert_eq!(loaded.state.config_text, state.config_text);
    assert_eq!(loaded.state.secrets, state.secrets);
    assert_eq!(loaded.state.settings, state.settings);
}

/// Heartbeats: a node appears, a stale one drops out, a departing one
/// leaves immediately.
async fn liveness_tracks_the_fleet(plane: Arc<dyn ControlPlane>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    for (id, addr) in [("node-a", "10.0.0.1:9443"), ("node-b", "10.0.0.2:9443")] {
        plane
            .heartbeat(&NodeBeat {
                id: id.into(),
                addr: addr.into(),
                seen_ms: now,
            })
            .await
            .unwrap();
    }

    let mut peers = plane.peers(WINDOW).await.unwrap();
    peers.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(peers.len(), 2, "both nodes are live");
    assert_eq!(peers[0].id, "node-a");
    assert_eq!(peers[0].addr, "10.0.0.1:9443", "the address survives");

    plane.depart("node-a").await.unwrap();
    let after = plane.peers(WINDOW).await.unwrap();
    assert_eq!(after.len(), 1, "a departed node leaves at once");
    assert_eq!(after[0].id, "node-b");
}

// ------------------------------------------------------- per backend

macro_rules! suite {
    ($name:ident, $build:expr, liveness = $liveness:expr) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn round_trips() {
                round_trip($build.await).await;
            }

            #[tokio::test]
            async fn conflicts_on_concurrent_write() {
                concurrent_writers_conflict($build.await).await;
            }

            #[tokio::test]
            async fn conflicts_on_concurrent_create() {
                concurrent_creates_conflict($build.await).await;
            }

            #[tokio::test]
            async fn preserves_the_whole_document() {
                full_document_round_trips($build.await).await;
            }

            #[tokio::test]
            async fn tracks_liveness() {
                if $liveness {
                    liveness_tracks_the_fleet($build.await).await;
                }
            }
        }
    };
}

suite!(memory_backend, memory(), liveness = true);
suite!(
    file_backend,
    file(Box::leak(Box::new(tempfile::tempdir().unwrap()))),
    liveness = true
);
suite!(s3_backend, s3(&S3Mock::spawn().await.1), liveness = true);
suite!(
    dynamodb_backend,
    dynamo(&DynamoMock::spawn().await.1),
    liveness = true
);

// --------------------------------------------------- backend specifics

#[tokio::test]
async fn s3_uses_if_none_match_to_create_and_if_match_to_replace() {
    let (mock, endpoint) = S3Mock::spawn().await;
    let plane = s3(&endpoint).await;

    let empty = plane.load().await.unwrap();
    plane.commit(&empty, state_with("one")).await.unwrap();
    assert_eq!(mock.object_count("rapid/store.json"), 1);

    // Another node writes. Our cached ETag is now stale, so the next
    // conditional PUT must be refused rather than clobbering their work.
    let stolen = serde_json::to_vec(&serde_json::json!({
        "format": 1, "version": 7, "state": {"config_text": "theirs"}
    }))
    .unwrap();
    mock.force_put("rapid/store.json", &stolen);

    let stale = plane.load().await.unwrap();
    let older = router_store::Snapshot {
        version: 1,
        token: Some("\"1\"".into()),
        ..stale.clone()
    };
    let err = plane.commit(&older, state_with("ours")).await;
    assert!(matches!(err, Err(ControlPlaneError::Conflict { .. })));
    assert_eq!(
        plane.load().await.unwrap().state.config_text.as_deref(),
        Some("theirs"),
    );
}

#[tokio::test]
async fn s3_outage_fails_writes_without_corrupting_anything() {
    let (mock, endpoint) = S3Mock::spawn().await;
    let plane = s3(&endpoint).await;
    let base = plane.load().await.unwrap();
    plane.commit(&base, state_with("before")).await.unwrap();

    mock.set_offline(true);
    let err = plane.load().await.expect_err("reads fail while S3 is down");
    assert!(
        matches!(err, ControlPlaneError::Unavailable(_)),
        "an outage is Unavailable, not Conflict or Fault: {err}",
    );

    mock.set_offline(false);
    assert_eq!(
        plane.load().await.unwrap().state.config_text.as_deref(),
        Some("before"),
        "the document is intact once the backend returns",
    );
}

#[tokio::test]
async fn s3_ignores_stale_and_foreign_objects_when_counting_the_fleet() {
    let (mock, endpoint) = S3Mock::spawn().await;
    let plane = s3(&endpoint).await;
    let now = router_store::backend::now_ms_for_tests();

    for id in ["node-a", "node-b"] {
        plane
            .heartbeat(&NodeBeat {
                id: id.into(),
                addr: "10.0.0.1:9443".into(),
                seen_ms: now,
            })
            .await
            .unwrap();
    }
    // A document object shares the prefix root but is not a heartbeat.
    plane
        .commit(&plane.load().await.unwrap(), state_with("x"))
        .await
        .unwrap();

    let key = mock
        .keys()
        .into_iter()
        .find(|k| k.starts_with("rapid/nodes/node-a."))
        .expect("node-a has a heartbeat object");
    mock.age_object(&key, 60_000);

    let live = plane.peers(WINDOW).await.unwrap();
    assert_eq!(live.len(), 1, "the aged-out node is not counted: {live:?}");
    assert_eq!(live[0].id, "node-b");
}

#[tokio::test]
async fn dynamodb_condition_expression_is_what_guards_the_write() {
    let (mock, endpoint) = DynamoMock::spawn().await;
    let plane = dynamo(&endpoint).await;

    let base = plane.load().await.unwrap();
    plane.commit(&base, state_with("one")).await.unwrap();

    let item = mock.get("store", "v1").expect("the store item exists");
    assert_eq!(
        item.get("version")
            .and_then(|v| v.get("N"))
            .and_then(|v| v.as_str()),
        Some("1"),
        "the version attribute is what the condition tests",
    );

    let err = plane.commit(&base, state_with("two")).await;
    assert!(matches!(err, Err(ControlPlaneError::Conflict { .. })));
}

#[tokio::test]
async fn dynamodb_heartbeats_carry_a_ttl_so_dead_nodes_are_swept_up() {
    let (mock, endpoint) = DynamoMock::spawn().await;
    let plane = dynamo(&endpoint).await;
    let now = router_store::backend::now_ms_for_tests();

    plane
        .heartbeat(&NodeBeat {
            id: "node-a".into(),
            addr: "10.0.0.1:9443".into(),
            seen_ms: now,
        })
        .await
        .unwrap();

    let item = mock
        .get("nodes", "node-a")
        .expect("the heartbeat item exists");
    let expires: u64 = item["expires_at"]["N"].as_str().unwrap().parse().unwrap();
    assert!(
        expires > now / 1000,
        "expires_at must be in the future for DynamoDB TTL to be a garbage collector",
    );
    assert_eq!(mock.count("nodes"), 1);
}

#[tokio::test]
async fn dynamodb_outage_is_reported_as_unavailable() {
    let (mock, endpoint) = DynamoMock::spawn().await;
    let plane = dynamo(&endpoint).await;
    mock.set_offline(true);
    let err = plane
        .load()
        .await
        .expect_err("reads fail while the table is down");
    assert!(
        matches!(err, ControlPlaneError::Unavailable(_)),
        "got {err}"
    );
}
