//! Multi-node consensus.
//!
//! The control plane is replicated by Raft; the data plane never touches
//! it. A single node is a cluster of one — the same log, the same apply
//! path, quorum of one — so adding boxes changes the membership, not the
//! design.
//!
//! Consensus itself is openraft's; what lives here is the storage the
//! algorithm writes through, the transport it speaks over, and the join
//! flow that lets an operator grow a cluster with one command.

mod network;
pub mod server;
mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openraft::{BasicNode, Config, Raft, TokioRuntime};
use serde::{Deserialize, Serialize};

use crate::state::{Command, StoreState};

pub use network::{PeerClient, PeerError, RaftRpc};
pub use store::{LogStore, StateMachineStore};

/// Nodes are identified by a stable u64 minted at first boot and kept in
/// `node.key`, so a node keeps its identity across restarts and disk
/// moves.
pub type NodeId = u64;

/// What a client write returns once committed and applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponse {
    pub version: u64,
}

openraft::declare_raft_types!(
    pub TypeConfig:
        D = Command,
        R = CommandResponse,
        NodeId = NodeId,
        Node = BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = TokioRuntime,
);

pub type ClusterRaft = Raft<TypeConfig>;

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("cluster is not initialized")]
    Uninitialized,
    #[error("not the leader; forward to {leader:?}")]
    NotLeader { leader: Option<String> },
    #[error(
        "config writes need a quorum and this node cannot reach one; traffic keeps flowing from the last applied state"
    )]
    NoQuorum,
    #[error("raft error: {0}")]
    Raft(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A running cluster member.
pub struct ClusterNode {
    pub raft: ClusterRaft,
    pub id: NodeId,
    pub addr: String,
    sm: Arc<StateMachineStore>,
    peers: PeerClient,
    /// Set by the server crate so peers can query this node's usage
    /// without router-cluster knowing what usage is.
    usage_responder: std::sync::OnceLock<UsageResponder>,
    /// When each peer last answered a probe. Membership is not liveness:
    /// a dead box stays in the config until an operator removes it, but
    /// its share of a rate limit must be redistributed within seconds.
    liveness: Mutex<HashMap<NodeId, Instant>>,
}

/// Answers a peer's usage query: takes the raw request body, returns the
/// JSON summary to send back.
pub type UsageResponder = Box<dyn Fn(&[u8]) -> serde_json::Value + Send + Sync>;

/// Tuning that matches the product claim: elections in milliseconds to
/// seconds, and a heartbeat frequent enough that live-member counts (and
/// therefore rate-limit shares) track reality within a second or two.
/// A peer must answer within this window to count as live. Three probe
/// intervals: long enough to ride out one lost packet, short enough that
/// shares rescale in a couple of seconds.
const LIVENESS_WINDOW: Duration = Duration::from_millis(1_500);
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// How long a leader may go without a quorum acknowledgement before we
/// treat writes as impossible. Generous next to the 100 ms heartbeat, so
/// a hiccup is not mistaken for a partition.
const QUORUM_ACK_WINDOW_MS: u64 = 2_000;

/// A config write that has not committed by now is not going to.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

fn raft_config() -> Config {
    Config {
        cluster_name: "caret-router".into(),
        election_timeout_min: 300,
        election_timeout_max: 600,
        heartbeat_interval: 100,
        // Snapshot after a modest number of entries: the control plane is
        // small and rarely written, so compaction is cheap and keeps a
        // joining node's catch-up short.
        snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(512),
        max_in_snapshot_log_to_keep: 128,
        ..Default::default()
    }
}

impl ClusterNode {
    /// Start the Raft node over `data_dir`. Does not join or bootstrap —
    /// the caller decides whether this node initializes a new cluster or
    /// joins an existing one.
    pub async fn start(
        id: NodeId,
        addr: String,
        data_dir: &Path,
        token: crate::token::JoinToken,
    ) -> Result<Arc<Self>, ClusterError> {
        let dir: PathBuf = data_dir.join("raft");
        std::fs::create_dir_all(&dir)?;

        let log = store::LogStore::open(&dir)?;
        let sm = Arc::new(StateMachineStore::open(&dir)?);
        let peers = PeerClient::new(token);

        let raft = Raft::new(
            id,
            Arc::new(raft_config().validate().expect("static config is valid")),
            peers.clone(),
            log,
            sm.clone(),
        )
        .await
        .map_err(|e| ClusterError::Raft(e.to_string()))?;

        Ok(Arc::new(Self {
            raft,
            id,
            addr,
            sm,
            peers,
            usage_responder: std::sync::OnceLock::new(),
            liveness: Mutex::new(HashMap::new()),
        }))
    }

    /// Bootstrap a brand-new single-voter cluster. Idempotent: a node that
    /// already has membership keeps it, so restarts never re-bootstrap.
    pub async fn bootstrap(&self) -> Result<(), ClusterError> {
        if self.is_initialized().await {
            return Ok(());
        }
        let mut members = BTreeMap::new();
        members.insert(self.id, BasicNode::new(self.addr.clone()));
        match self.raft.initialize(members).await {
            Ok(()) => Ok(()),
            // Losing the race with another initializer is success.
            Err(e) if e.to_string().contains("already initialized") => Ok(()),
            Err(e) => Err(ClusterError::Raft(e.to_string())),
        }
    }

    pub async fn is_initialized(&self) -> bool {
        !self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .count()
            .eq(&0)
    }

    /// Commit a command through consensus. On a single node this is a log
    /// append and an apply; on a cluster it is a quorum round-trip.
    ///
    /// A write that cannot reach a quorum fails with a clear error rather
    /// than hanging: an operator editing config on a partitioned node has
    /// to be told, not left waiting.
    pub async fn commit(&self, command: Command) -> Result<u64, ClusterError> {
        if !self.has_quorum() {
            return Err(ClusterError::NoQuorum);
        }
        match tokio::time::timeout(WRITE_TIMEOUT, self.raft.client_write(command.clone())).await {
            Ok(Ok(response)) => Ok(response.data.version),
            Ok(Err(err)) => {
                let leader = self.leader_addr().await;
                match classify(err.to_string(), leader.clone()) {
                    // Any node accepts a write and forwards it — the
                    // console has to work the same on whichever box the
                    // load balancer picked.
                    ClusterError::NotLeader { .. } => match leader {
                        Some(addr) => self.forward(&addr, command).await,
                        None => Err(ClusterError::NoQuorum),
                    },
                    other => Err(other),
                }
            }
            Err(_) => Err(ClusterError::NoQuorum),
        }
    }

    /// Hand a write to the leader over the cluster port and return what it
    /// committed.
    async fn forward(&self, leader: &str, command: Command) -> Result<u64, ClusterError> {
        #[derive(serde::Deserialize)]
        struct Committed {
            version: u64,
        }
        self.peers
            .post::<_, Committed>(leader, "/cluster/commit", &command, WRITE_TIMEOUT)
            .await
            .map(|c| c.version)
            .map_err(|err| match err {
                PeerError::Status { body, .. } if body.contains("quorum") => ClusterError::NoQuorum,
                other => ClusterError::Raft(format!("forwarding to leader failed: {other}")),
            })
    }

    /// Whether this node can currently commit. A leader must have heard
    /// from a quorum recently; anyone else needs a leader to forward to.
    pub fn has_quorum(&self) -> bool {
        let metrics = self.raft.metrics().borrow().clone();
        if metrics.current_leader != Some(self.id) {
            return metrics.current_leader.is_some();
        }
        // A lone leader in a partition still calls itself leader until it
        // steps down; the honest signal is when a quorum last acked.
        let voters = metrics.membership_config.membership().voter_ids().count();
        if voters <= 1 {
            return true; // a cluster of one is its own quorum
        }
        metrics
            .millis_since_quorum_ack
            .is_none_or(|ms| ms < QUORUM_ACK_WINDOW_MS)
    }

    /// A point-in-time copy of the replicated state, with the log index it
    /// reflects. This is a local read — the data plane never blocks on
    /// consensus.
    pub fn read(&self) -> (StoreState, u64) {
        self.sm.read()
    }

    pub fn state_machine(&self) -> Arc<StateMachineStore> {
        self.sm.clone()
    }

    pub async fn leader_addr(&self) -> Option<String> {
        let metrics = self.raft.metrics().borrow().clone();
        let leader = metrics.current_leader?;
        metrics
            .membership_config
            .membership()
            .get_node(&leader)
            .map(|n| n.addr.clone())
    }

    /// Add a node as a voter: openraft streams it a snapshot, catches it
    /// up from the log, then commits the membership change through joint
    /// consensus. No downtime, no manual quorum arithmetic.
    ///
    /// Re-announcing an existing member updates its address instead. That
    /// is the rejoin path: a box that lost its disk, or moved, comes back
    /// with the same identity on a new endpoint, and the cluster has to
    /// start talking to the new one rather than retrying the old forever.
    pub async fn add_voter(&self, id: NodeId, addr: String) -> Result<(), ClusterError> {
        let (known, stale_at_addr) = {
            let metrics = self.raft.metrics();
            let membership = metrics.borrow();
            let membership = membership.membership_config.membership();
            let known = membership.get_node(&id).cloned();
            // A different id already sitting on this address means the box
            // came back with a new identity — which is exactly what a lost
            // disk produces, because the node key is regenerated with it.
            let stale: Vec<NodeId> = membership
                .nodes()
                .filter(|(other, node)| **other != id && node.addr == addr)
                .map(|(other, _)| *other)
                .collect();
            (known, stale)
        };

        // Retire the ghost first. Leaving it in membership would inflate
        // the quorum a live fleet has to reach, and re-admitting an
        // empty-log node under the old id is a Raft durability violation
        // (openraft rejects the log reversion outright).
        for ghost in stale_at_addr {
            tracing::info!(
                %ghost, %addr,
                "replacing a member that returned with a new identity"
            );
            self.remove_voter(ghost).await?;
        }

        if let Some(existing) = known {
            if existing.addr != addr {
                // Same id, new endpoint. `SetNodes` is the only way to
                // rewrite it; it is safe here precisely because the id is
                // the node's own stable identity, not one we invented.
                let mut nodes = BTreeMap::new();
                nodes.insert(id, BasicNode::new(addr));
                self.raft
                    .change_membership(openraft::ChangeMembers::SetNodes(nodes), false)
                    .await
                    .map_err(|e| classify(e.to_string(), None))?;
            }
            // Already a member at this address: nothing to do. Joining
            // twice must be harmless so operators can re-run the command.
            return Ok(());
        }

        self.raft
            .add_learner(id, BasicNode::new(addr), true)
            .await
            .map_err(|e| classify(e.to_string(), None))?;

        let mut voters: BTreeSet<NodeId> = self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        voters.insert(id);
        self.raft
            .change_membership(voters, false)
            .await
            .map_err(|e| classify(e.to_string(), None))?;
        Ok(())
    }

    /// Remove a node from membership — for a box that is never coming
    /// back. The remaining voters must still form a quorum.
    pub async fn remove_voter(&self, id: NodeId) -> Result<(), ClusterError> {
        let mut voters: BTreeSet<NodeId> = self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        if !voters.remove(&id) {
            return Ok(());
        }
        if voters.is_empty() {
            return Err(ClusterError::Raft(
                "refusing to remove the last voter — that would destroy the cluster".into(),
            ));
        }
        self.raft
            .change_membership(voters, false)
            .await
            .map_err(|e| classify(e.to_string(), None))?;
        Ok(())
    }

    /// The fleet view the console and `cluster status` render.
    pub async fn fleet(&self) -> Fleet {
        let metrics = self.raft.metrics().borrow().clone();
        let membership = metrics.membership_config.membership().clone();
        let voters: BTreeSet<NodeId> = membership.voter_ids().collect();
        let applied = metrics.last_applied.map(|l| l.index).unwrap_or(0);

        let mut members = Vec::new();
        for (id, node) in membership.nodes() {
            let replication = metrics
                .replication
                .as_ref()
                .and_then(|r| r.get(id))
                .and_then(|o| o.as_ref())
                .map(|l| l.index);
            members.push(Member {
                id: *id,
                addr: node.addr.clone(),
                voter: voters.contains(id),
                leader: metrics.current_leader == Some(*id),
                is_self: *id == self.id,
                // The leader knows every follower's match index; a
                // follower only knows its own.
                applied: if *id == self.id {
                    Some(applied)
                } else {
                    replication
                },
                lag: replication.map(|idx| applied.saturating_sub(idx)),
            });
        }
        members.sort_by_key(|m| m.id);

        let live = self.live_members();
        Fleet {
            node_id: self.id,
            leader: metrics.current_leader,
            term: metrics.current_term,
            applied,
            quorum: self.has_quorum(),
            voters: voters.len(),
            live,
            members,
        }
    }

    /// How many members are alive right now — the denominator for
    /// per-node rate-limit shares.
    ///
    /// This counts peers that answered a probe recently, plus ourselves.
    /// Using membership instead would leave a dead node holding a share
    /// of every limit until someone noticed.
    pub fn live_members(&self) -> usize {
        let members: Vec<NodeId> = self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| *id)
            .filter(|id| *id != self.id)
            .collect();
        let seen = self.liveness.lock().unwrap();
        let live_peers = members
            .iter()
            .filter(|id| {
                seen.get(id)
                    .is_some_and(|at| at.elapsed() < LIVENESS_WINDOW)
            })
            .count();
        live_peers + 1
    }

    /// Probe peers on a short interval so `live_members` reflects reality
    /// within a couple of heartbeats.
    pub fn spawn_liveness_probe(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                let members: Vec<(NodeId, String)> = node
                    .raft
                    .metrics()
                    .borrow()
                    .membership_config
                    .membership()
                    .nodes()
                    .filter(|(id, _)| **id != node.id)
                    .map(|(id, n)| (*id, n.addr.clone()))
                    .collect();
                for (id, addr) in members {
                    let reachable = node
                        .peers
                        .post::<_, serde_json::Value>(
                            &addr,
                            "/cluster/ping",
                            &serde_json::json!({}),
                            PROBE_TIMEOUT,
                        )
                        .await
                        .is_ok();
                    if reachable {
                        node.liveness.lock().unwrap().insert(id, Instant::now());
                    }
                }
            }
        });
    }

    pub fn peers(&self) -> &PeerClient {
        &self.peers
    }

    /// Install the callback that answers peer usage queries. Called once,
    /// by the server that owns the usage pipeline.
    pub fn set_usage_responder(&self, responder: UsageResponder) {
        let _ = self.usage_responder.set(responder);
    }

    pub fn usage_responder(&self) -> Option<&UsageResponder> {
        self.usage_responder.get()
    }

    /// Addresses of every member except this one — the scatter-gather
    /// fan-out set.
    pub fn peer_addrs(&self) -> Vec<String> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .filter(|(id, _)| **id != self.id)
            .map(|(_, node)| node.addr.clone())
            .collect()
    }

    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
    }
}

/// Map openraft's error text onto the operator-facing distinction that
/// matters: "ask another node" versus "the cluster cannot commit".
fn classify(message: String, leader: Option<String>) -> ClusterError {
    let lower = message.to_lowercase();
    if lower.contains("forwardtoleader") || lower.contains("has to forward request to") {
        return ClusterError::NotLeader { leader };
    }
    if lower.contains("quorum") || lower.contains("no leader") || lower.contains("timeout") {
        return ClusterError::NoQuorum;
    }
    ClusterError::Raft(message)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub node_id: NodeId,
    pub leader: Option<NodeId>,
    pub term: u64,
    pub applied: u64,
    pub quorum: bool,
    pub voters: usize,
    pub live: usize,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: NodeId,
    pub addr: String,
    pub voter: bool,
    pub leader: bool,
    pub is_self: bool,
    pub applied: Option<u64>,
    pub lag: Option<u64>,
}
