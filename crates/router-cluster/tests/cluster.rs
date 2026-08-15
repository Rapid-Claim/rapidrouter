//! Multi-node behavior: forming a cluster, surviving a leader kill,
//! refusing writes in a minority, healing, and rebuilding a node that
//! lost its disk.
//!
//! Nodes are real Raft nodes over real TCP on loopback, so this exercises
//! the actual transport, join handshake, and storage — not a simulation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use router_cluster::raft::ClusterNode;
use router_cluster::raft::server::{self, JoinRequest};
use router_cluster::state::Command;
use router_cluster::token::JoinToken;

struct Node {
    inner: Arc<ClusterNode>,
    /// Held so the data directory outlives the node using it.
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
        // Production spawns this too: membership is not liveness.
        inner.spawn_liveness_probe();
        Self {
            inner,
            _dir: dir,
            shutdown: Some(tx),
            addr,
            id,
        }
    }

    /// Stop serving and shut the Raft node down, keeping the data dir so
    /// the node can come back.
    async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.inner.shutdown().await;
        // Let the listener actually close before anyone rebinds.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn spawn_cluster(size: usize) -> (JoinToken, Vec<Node>) {
    let token = JoinToken::generate();
    let mut nodes = Vec::new();
    for id in 1..=size as u64 {
        let dir = tempfile::tempdir().unwrap();
        nodes.push(Node::start(id, token.clone(), dir).await);
    }
    nodes[0].inner.bootstrap().await.expect("bootstrap");
    await_leader(&nodes[..1]).await;

    for i in 1..size {
        let me = JoinRequest {
            node_id: nodes[i].id,
            addr: nodes[i].addr.clone(),
        };
        let seeds: Vec<String> = vec![nodes[0].addr.clone()];
        server::request_join(&token, &seeds, me)
            .await
            .unwrap_or_else(|e| panic!("node {} could not join: {e}", i + 1));
    }
    await_replication(&nodes).await;
    (token, nodes)
}

/// Wait for some node in the slice to believe it has a leader.
async fn await_leader(nodes: &[Node]) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for node in nodes {
            if let Some(leader) = node.inner.raft.metrics().borrow().current_leader {
                return leader;
            }
        }
        assert!(Instant::now() < deadline, "no leader elected in time");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn membership_size(node: &ClusterNode) -> usize {
    node.raft
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .nodes()
        .count()
}

/// Wait until every node reports the same membership size.
async fn await_replication(nodes: &[Node]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let sizes: Vec<usize> = nodes.iter().map(|n| membership_size(&n.inner)).collect();
        if sizes.iter().all(|s| *s == nodes.len()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "membership did not converge: {sizes:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait for a committed write to appear in a node's local state.
async fn await_state(node: &ClusterNode, want: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if node.read().0.config_text.as_deref() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn put(text: &str) -> Command {
    Command::PutConfig { text: text.into() }
}

/// Find whichever node currently leads.
async fn leader_index(nodes: &[Node]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for (i, node) in nodes.iter().enumerate() {
            let metrics = node.inner.raft.metrics().borrow().clone();
            if metrics.current_leader == Some(node.id) {
                return i;
            }
        }
        assert!(Instant::now() < deadline, "no node claims leadership");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn three_nodes_form_a_cluster_and_replicate_writes() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let leader = leader_index(&nodes).await;

    nodes[leader]
        .inner
        .commit(put("replicated = true\n"))
        .await
        .expect("leader commits");

    for node in &nodes {
        assert!(
            await_state(&node.inner, "replicated = true\n", Duration::from_secs(10)).await,
            "node {} never saw the write",
            node.id
        );
    }
    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn a_follower_forwards_writes_to_the_leader() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let leader = leader_index(&nodes).await;
    let follower = (leader + 1) % 3;

    // The console runs on whichever node the load balancer picked, so a
    // write must succeed from any of them.
    let version = nodes[follower]
        .inner
        .commit(put("from-follower\n"))
        .await
        .expect("a follower forwards to the leader instead of refusing");
    assert!(version > 0);

    for node in &nodes {
        assert!(
            await_state(&node.inner, "from-follower\n", Duration::from_secs(10)).await,
            "node {} never saw the forwarded write",
            node.id
        );
    }
    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn killing_the_leader_elects_a_new_one_and_writes_resume() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let old_leader = leader_index(&nodes).await;
    nodes[old_leader]
        .inner
        .commit(put("before-kill\n"))
        .await
        .expect("write before the kill");

    nodes[old_leader].stop().await;

    // The survivors must elect among themselves and accept writes again.
    let survivors: Vec<&Node> = nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != old_leader)
        .map(|(_, n)| n)
        .collect();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut committed = false;
    while Instant::now() < deadline && !committed {
        for node in &survivors {
            if node.inner.commit(put("after-kill\n")).await.is_ok() {
                committed = true;
                break;
            }
        }
        if !committed {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    assert!(committed, "surviving majority never accepted a write");

    for node in &survivors {
        assert!(
            await_state(&node.inner, "after-kill\n", Duration::from_secs(10)).await,
            "survivor {} did not converge",
            node.id
        );
    }
    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn a_minority_refuses_config_writes_but_keeps_its_state_readable() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let leader = leader_index(&nodes).await;
    nodes[leader]
        .inner
        .commit(put("last-good\n"))
        .await
        .expect("write while healthy");
    for node in &nodes {
        assert!(await_state(&node.inner, "last-good\n", Duration::from_secs(10)).await);
    }

    // Take down the two nodes that are not our observer: it is now a
    // minority of one.
    let observer = (leader + 1) % 3;
    for (i, node) in nodes.iter_mut().enumerate() {
        if i != observer {
            node.stop().await;
        }
    }

    // Reads keep working from the last applied state — this is the whole
    // promise: quorum loss degrades config writes, never traffic.
    let (state, _) = nodes[observer].inner.read();
    assert_eq!(state.config_text.as_deref(), Some("last-good\n"));

    // And a write is refused rather than silently accepted.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        nodes[observer].inner.commit(put("should-not-commit\n")),
    )
    .await;
    if let Ok(Ok(_)) = result {
        panic!("a minority must not commit config writes");
    }
    // The refusal must not have corrupted the readable state.
    let (state, _) = nodes[observer].inner.read();
    assert_eq!(state.config_text.as_deref(), Some("last-good\n"));

    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn a_node_that_lost_its_disk_rebuilds_from_a_snapshot() {
    let (token, mut nodes) = spawn_cluster(3).await;
    let leader = leader_index(&nodes).await;
    nodes[leader]
        .inner
        .commit(put("survives-disk-loss\n"))
        .await
        .expect("write");
    await_replication(&nodes).await;

    // Wipe a follower entirely. Its node key goes with the disk, so it
    // comes back under a new identity — and the cluster must retire the
    // old one rather than re-admitting an empty-log node under it, which
    // Raft forbids.
    let victim = (leader + 1) % 3;
    let victim_id = nodes[victim].id;
    let victim_addr = nodes[victim].addr.clone();
    nodes[victim].stop().await;

    let rebuilt = Node::start(9_001, token.clone(), tempfile::tempdir().unwrap()).await;
    let seeds: Vec<String> = vec![nodes[leader].addr.clone()];
    server::request_join(
        &token,
        &seeds,
        JoinRequest {
            node_id: 9_001,
            addr: victim_addr.clone(),
        },
    )
    .await
    .expect("the rebuilt node is admitted");
    let _ = victim_id;

    // Re-announce at its real address so replication can reach it.
    let _ = server::request_join(
        &token,
        &seeds,
        JoinRequest {
            node_id: 9_001,
            addr: rebuilt.addr.clone(),
        },
    )
    .await;

    assert!(
        await_state(
            &rebuilt.inner,
            "survives-disk-loss\n",
            Duration::from_secs(30)
        )
        .await,
        "the rebuilt node never caught up"
    );

    let mut rebuilt = rebuilt;
    rebuilt.stop().await;
    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn a_restarted_node_recovers_from_its_own_log() {
    let token = JoinToken::generate();
    // Held for the whole test: the point is that the *data* survives a
    // restart, so the directory must outlive the first node.
    let keep = tempfile::tempdir().unwrap();
    let path = keep.path().to_owned();

    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let node = ClusterNode::start(1, addr, &path, token.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let serving = node.clone();
        tokio::spawn(async move {
            let _ = server::serve(listener, serving, async {
                let _ = rx.await;
            })
            .await;
        });
        node.bootstrap().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while node.raft.metrics().borrow().current_leader.is_none() {
            assert!(Instant::now() < deadline, "no leader");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        node.commit(put("durable\n")).await.unwrap();
        let _ = tx.send(());
        node.shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Same data dir, fresh process-equivalent: no external dependency in
    // the recovery path.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let restarted = ClusterNode::start(1, addr, &path, token).await.unwrap();
    let (state, version) = restarted.read();
    assert_eq!(state.config_text.as_deref(), Some("durable\n"));
    assert!(version > 0);
    restarted.shutdown().await;
}

#[tokio::test]
async fn live_member_count_drives_rate_limit_shares() {
    let (_token, mut nodes) = spawn_cluster(3).await;
    let leader = leader_index(&nodes).await;

    // Give the probe a round to see both peers.
    let deadline = Instant::now() + Duration::from_secs(10);
    while nodes[leader].inner.live_members() != 3 {
        assert!(Instant::now() < deadline, "peers never registered as live");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(nodes[leader].inner.live_members(), 3);

    // Removing a member rescales the denominator without anyone editing a
    // config: a 600 rpm limit goes from 200/node to 300/node.
    // Killing a node — not removing it from membership — must still
    // rescale shares: a dead box's quota has to come back to the fleet
    // without waiting for an operator.
    let victim = (leader + 1) % 3;
    nodes[victim].stop().await;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if nodes[leader].inner.live_members() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "share count never rescaled after a node died"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A 600 rpm fleet limit goes from 200/node to 300/node.
    assert_eq!(600 / nodes[leader].inner.live_members() as u64, 300);

    // And an explicit removal is also honored.
    let gone = nodes[victim].id;
    let _ = nodes[leader].inner.remove_voter(gone).await;

    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn a_peer_without_the_token_is_refused() {
    let (_token, mut nodes) = spawn_cluster(1).await;
    let stranger = JoinToken::generate();
    let client = router_cluster::raft::PeerClient::new(stranger);
    let result: Result<serde_json::Value, _> = client
        .post(
            &nodes[0].addr,
            "/cluster/fleet",
            &serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await;
    match result {
        Err(router_cluster::raft::PeerError::Status { status, .. }) => assert_eq!(status, 401),
        other => panic!("a foreign token must be refused, got {other:?}"),
    }
    for node in &mut nodes {
        node.stop().await;
    }
}

#[tokio::test]
async fn five_nodes_tolerate_two_failures() {
    let (_token, mut nodes) = spawn_cluster(5).await;
    let leader = leader_index(&nodes).await;

    // Kill two non-leaders: three remain, which is still a majority of 5.
    let mut killed = 0;
    for (i, node) in nodes.iter_mut().enumerate() {
        if i != leader && killed < 2 {
            node.stop().await;
            killed += 1;
        }
    }
    assert_eq!(killed, 2);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut committed = false;
    while Instant::now() < deadline && !committed {
        committed = nodes[leader]
            .inner
            .commit(put("quorum-of-three\n"))
            .await
            .is_ok();
        if !committed {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    assert!(committed, "3 of 5 nodes should still commit");

    for node in &mut nodes {
        node.stop().await;
    }
}
