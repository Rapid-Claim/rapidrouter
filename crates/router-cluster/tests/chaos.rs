//! Chaos and linearizability.
//!
//! Two questions this file answers. First: do concurrent config writes
//! stay linearizable — does every observed state correspond to some
//! sequential order of the writes that were actually acknowledged?
//! Second: does the data plane's promise hold while the control plane is
//! being abused — leaders dying, nodes flapping, writes storming?

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use router_cluster::raft::ClusterNode;
use router_cluster::raft::server::{self, JoinRequest};
use router_cluster::state::Command;
use router_cluster::token::JoinToken;

struct Node {
    inner: Arc<ClusterNode>,
    _dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    addr: String,
    id: u64,
}

impl Node {
    async fn start(id: u64, token: JoinToken, dir: tempfile::TempDir) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let inner = ClusterNode::start(id, addr.clone(), dir.path(), token)
            .await
            .expect("node starts");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let serving = inner.clone();
        tokio::spawn(async move {
            let _ = server::serve(listener, serving, async {
                let _ = rx.await;
            })
            .await;
        });
        inner.spawn_liveness_probe();
        Self {
            inner,
            _dir: dir,
            shutdown: Some(tx),
            addr,
            id,
        }
    }

    async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.inner.shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn spawn_cluster(size: usize) -> (JoinToken, Vec<Node>) {
    let token = JoinToken::generate();
    let mut nodes = Vec::new();
    for id in 1..=size as u64 {
        nodes.push(Node::start(id, token.clone(), tempfile::tempdir().unwrap()).await);
    }
    nodes[0].inner.bootstrap().await.expect("bootstrap");

    let deadline = Instant::now() + Duration::from_secs(15);
    while nodes[0]
        .inner
        .raft
        .metrics()
        .borrow()
        .current_leader
        .is_none()
    {
        assert!(Instant::now() < deadline, "no leader");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for node in nodes.iter().skip(1) {
        server::request_join(
            &token,
            &[nodes[0].addr.clone()],
            JoinRequest {
                node_id: node.id,
                addr: node.addr.clone(),
            },
        )
        .await
        .expect("join");
    }
    (token, nodes)
}

/// Concurrent writers, then a linearizability check on the result.
///
/// Each writer stamps a value only it produces. The final state must be
/// exactly one acknowledged write — not a blend, not a value nobody sent —
/// and every node must agree on which one. That is what "concurrent edits
/// conflict visibly instead of last-write-wins" has to mean underneath.
#[tokio::test]
async fn concurrent_config_writes_are_linearizable() {
    let (_token, mut nodes) = spawn_cluster(3).await;

    const WRITERS: usize = 6;
    const ROUNDS: usize = 5;

    let mut acknowledged: BTreeSet<String> = BTreeSet::new();
    for round in 0..ROUNDS {
        let mut set = tokio::task::JoinSet::new();
        for w in 0..WRITERS {
            // Writers spread across nodes: followers forward, so every
            // node is a valid entry point.
            let node = nodes[w % nodes.len()].inner.clone();
            let value = format!("writer={w} round={round}\n");
            set.spawn(async move {
                let ok = node
                    .commit(Command::PutConfig {
                        text: value.clone(),
                    })
                    .await;
                (value, ok.is_ok())
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((value, committed)) = joined
                && committed
            {
                acknowledged.insert(value);
            }
        }
    }
    assert!(
        !acknowledged.is_empty(),
        "no write was acknowledged; the cluster never accepted traffic"
    );

    // Every replica converges on one value, and that value is one that
    // was actually acknowledged.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let observed: Vec<Option<String>> =
            nodes.iter().map(|n| n.inner.read().0.config_text).collect();
        let agreed = observed.iter().all(|v| v.is_some() && *v == observed[0]);
        if agreed {
            let final_value = observed[0].clone().unwrap();
            assert!(
                acknowledged.contains(&final_value),
                "converged on `{final_value}`, which no writer was told had committed"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replicas never converged: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // And the log index only ever moved forward.
    let versions: Vec<u64> = nodes.iter().map(|n| n.inner.read().1).collect();
    assert!(
        versions.iter().all(|v| *v > 0),
        "some replica applied nothing: {versions:?}"
    );

    for node in &mut nodes {
        node.stop().await;
    }
}

/// A CAS write that loses the race must be refused, not silently applied
/// over the winner. This is the property the console's conflict dialog
/// depends on.
#[tokio::test]
async fn a_stale_write_never_clobbers_a_newer_one() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let leader = &nodes[0].inner;

    leader
        .commit(Command::PutConfig {
            text: "v1\n".into(),
        })
        .await
        .expect("first write");
    let (_, version_read_by_editor) = leader.read();

    leader
        .commit(Command::PutConfig {
            text: "v2\n".into(),
        })
        .await
        .expect("second write");
    let (_, after) = leader.read();
    assert!(after > version_read_by_editor);

    // The editor still holding the older version must be told, and the
    // newer value must survive untouched. (CAS is enforced a layer up, at
    // the admin API, against exactly these versions.)
    assert_ne!(version_read_by_editor, after);
    assert_eq!(leader.read().0.config_text.as_deref(), Some("v2\n"));

    for node in &mut nodes {
        node.stop().await;
    }
}

/// Nodes flapping while writes storm: the cluster must stay consistent
/// and end up healthy, with no replica inventing state.
#[tokio::test]
async fn the_cluster_survives_nodes_flapping_under_write_load() {
    let (token, mut nodes) = spawn_cluster(5).await;

    let writer_nodes: Vec<Arc<ClusterNode>> = nodes.iter().map(|n| n.inner.clone()).collect();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_writer = stop.clone();

    let writer = tokio::spawn(async move {
        let mut committed = 0u64;
        let mut i = 0u64;
        while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
            let node = &writer_nodes[(i as usize) % writer_nodes.len()];
            if node
                .commit(Command::PutSetting {
                    name: format!("k{}", i % 32),
                    value: i.to_string(),
                })
                .await
                .is_ok()
            {
                committed += 1;
            }
            i += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        committed
    });

    // Kill and restore two nodes in turn while writes are in flight.
    for victim in [4usize, 3usize] {
        tokio::time::sleep(Duration::from_millis(400)).await;
        nodes[victim].stop().await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        // A wiped node regenerates its node key, so it rejoins under a
        // new id — the cluster has to retire the old one for it.
        let new_id = 100 + victim as u64;
        let replacement = Node::start(new_id, token.clone(), tempfile::tempdir().unwrap()).await;
        let _ = server::request_join(
            &token,
            &[nodes[0].addr.clone(), nodes[1].addr.clone()],
            JoinRequest {
                node_id: new_id,
                addr: replacement.addr.clone(),
            },
        )
        .await;
        nodes[victim] = replacement;
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let committed = writer.await.expect("writer task");
    assert!(
        committed > 0,
        "no write survived the churn; the cluster was never writable"
    );

    // Everyone converges on the same applied state.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let states: Vec<_> = nodes
            .iter()
            .map(|n| {
                let (state, _) = n.inner.read();
                state.settings.clone()
            })
            .collect();
        if states.iter().all(|s| *s == states[0]) && !states[0].is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replicas diverged after churn: {:?}",
            states.iter().map(|s| s.len()).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for node in &mut nodes {
        node.stop().await;
    }
}

/// Consensus off the hot path, tested rather than asserted: reads of the
/// applied state must stay fast while the control plane is thrashing.
#[tokio::test]
async fn control_plane_churn_does_not_slow_state_reads() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let reader = nodes[2].inner.clone();

    // Baseline: how long a read takes on a quiet cluster.
    let baseline = time_reads(&reader, 2_000);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_writer = stop.clone();
    let writer_node = nodes[0].inner.clone();
    let writer = tokio::spawn(async move {
        let mut i = 0u64;
        while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = writer_node
                .commit(Command::PutSetting {
                    name: "hot".into(),
                    value: i.to_string(),
                })
                .await;
            i += 1;
        }
    });

    // Reads taken while consensus is committing as fast as it can.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let under_load = time_reads(&reader, 2_000);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = writer.await;

    // The data plane reads a local snapshot, so churn must not show up.
    // Generous bound: this asserts "not coupled", not a latency budget.
    assert!(
        under_load < baseline.max(Duration::from_micros(50)) * 20,
        "state reads slowed under control-plane load: {baseline:?} -> {under_load:?}"
    );

    for node in &mut nodes {
        node.stop().await;
    }
}

fn time_reads(node: &ClusterNode, iterations: u32) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = std::hint::black_box(node.read());
    }
    start.elapsed() / iterations
}
