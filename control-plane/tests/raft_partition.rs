use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use control_plane::{
    NodeConfig, Peer, RaftNode, app,
    link_proxy::{LinkMode, LinkProxy, LinkProxyConfig, LinkStatus, link_proxy_app},
    model::{NodeStatus, Role, RoutingConfiguration, WorkerConfiguration},
};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle, time::sleep};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inferlab-real-link-{}-{name}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct RunningCluster {
    directory: TestDirectory,
    nodes: HashMap<String, Arc<RaftNode>>,
    node_urls: HashMap<String, String>,
    link_urls: HashMap<String, String>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for RunningCluster {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl RunningCluster {
    async fn start() -> Self {
        let directory = TestDirectory::new("partition");
        let mut node_listeners = HashMap::new();
        let mut node_urls = HashMap::new();
        for node_id in ["node-a", "node-b", "node-c"] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind node listener");
            node_urls.insert(
                node_id.to_owned(),
                format!("http://{}", listener.local_addr().expect("node address")),
            );
            node_listeners.insert(node_id.to_owned(), listener);
        }

        let mut link_listeners = HashMap::new();
        let mut link_urls = HashMap::new();
        for source in ["node-a", "node-b", "node-c"] {
            for target in ["node-a", "node-b", "node-c"] {
                if source == target {
                    continue;
                }
                let link_id = format!("{source}-to-{target}");
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind link listener");
                link_urls.insert(
                    link_id.clone(),
                    format!("http://{}", listener.local_addr().expect("link address")),
                );
                link_listeners.insert(link_id, listener);
            }
        }

        let mut nodes = HashMap::new();
        for (node_id, election_min_ms, election_max_ms) in [
            ("node-a", 120, 180),
            ("node-b", 300, 380),
            ("node-c", 450, 530),
        ] {
            let peers = ["node-a", "node-b", "node-c"]
                .into_iter()
                .filter(|candidate| *candidate != node_id)
                .map(|peer_id| Peer {
                    id: peer_id.to_owned(),
                    base_url: link_urls[&format!("{node_id}-to-{peer_id}")].clone(),
                })
                .collect();
            let node_directory = directory.0.join(node_id);
            fs::create_dir_all(&node_directory).expect("create node directory");
            let node = RaftNode::open(NodeConfig {
                node_id: node_id.to_owned(),
                cluster_id: "inferlab-partition-test".to_owned(),
                peers,
                state_path: node_directory.join("state.json"),
                event_path: node_directory.join("events.jsonl"),
                election_timeout_min: Duration::from_millis(election_min_ms),
                election_timeout_max: Duration::from_millis(election_max_ms),
                heartbeat_interval: Duration::from_millis(35),
                rpc_timeout: Duration::from_millis(80),
                commit_timeout: Duration::from_millis(800),
            })
            .expect("open Raft node");
            nodes.insert(node_id.to_owned(), node);
        }

        let mut tasks = Vec::new();
        for (node_id, node) in &nodes {
            let listener = node_listeners.remove(node_id).expect("node listener");
            let router = app(Arc::clone(node));
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, router)
                    .await
                    .expect("serve Raft node");
            }));
        }

        for (link_id, listener) in link_listeners {
            let (source_id, target_id) =
                link_id.split_once("-to-").expect("directed link identity");
            let proxy = LinkProxy::open(LinkProxyConfig {
                link_id: link_id.clone(),
                source_id: source_id.to_owned(),
                target_id: target_id.to_owned(),
                upstream_base_url: node_urls[target_id].clone(),
                event_path: directory.0.join(format!("{link_id}.jsonl")),
            })
            .expect("open link proxy");
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, link_proxy_app(proxy))
                    .await
                    .expect("serve link proxy");
            }));
        }
        // Prove that all nine routers are accepting before any election loop
        // starts. Otherwise a loaded test runner can turn server startup order
        // into an accidental initial partition.
        let readiness_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(250))
            .build()
            .expect("readiness client");
        for base_url in node_urls.values().chain(link_urls.values()) {
            let mut ready = false;
            for _ in 0..100 {
                if readiness_client
                    .get(format!("{base_url}/healthz"))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    ready = true;
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            assert!(ready, "router at {base_url} did not become ready");
        }

        // Start A alone only for the deterministic fixture bootstrap. B and C
        // already serve vote/append RPCs, so A still wins through real Raft
        // requests and a real majority; their election loops begin only after
        // the initial leadership term is established. The exact term number is
        // intentionally not assumed because an overloaded runner can make one
        // bounded RPC round miss without changing the elected leader.
        tasks.push(nodes["node-a"].spawn_background());
        let mut node_a_elected = false;
        for _ in 0..300 {
            if nodes["node-a"]
                .status()
                .is_ok_and(|status| status.role == Role::Leader)
            {
                node_a_elected = true;
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            node_a_elected,
            "node-a did not establish fixture leadership"
        );
        for node_id in ["node-b", "node-c"] {
            tasks.push(nodes[node_id].spawn_background());
        }

        Self {
            directory,
            nodes,
            node_urls,
            link_urls,
            tasks,
        }
    }

    fn node(&self, node_id: &str) -> &Arc<RaftNode> {
        &self.nodes[node_id]
    }

    fn state_path(&self, node_id: &str) -> PathBuf {
        self.directory.0.join(node_id).join("state.json")
    }

    async fn set_link_mode(
        &self,
        client: &reqwest::Client,
        source_id: &str,
        target_id: &str,
        mode: LinkMode,
        reason: &str,
    ) -> LinkStatus {
        let link_id = format!("{source_id}-to-{target_id}");
        let response = client
            .put(format!("{}/v1/link/mode", self.link_urls[&link_id]))
            .json(&serde_json::json!({"mode": mode, "reason": reason}))
            .send()
            .await
            .expect("change link mode");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.json().await.expect("link status response")
    }
}

fn routing(policy: &str) -> RoutingConfiguration {
    RoutingConfiguration {
        routing_policy: policy.to_owned(),
        workers: vec![WorkerConfiguration {
            id: "worker-a".to_owned(),
            base_url: "http://127.0.0.1:9001".to_owned(),
            weight: 1,
        }],
    }
}

async fn wait_until(description: &str, mut predicate: impl FnMut() -> bool) -> Result<(), String> {
    for _ in 0..300 {
        if predicate() {
            return Ok(());
        }
        sleep(Duration::from_millis(20)).await;
    }
    Err(format!("timed out waiting for {description}"))
}

fn read_state(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read durable Raft state"))
        .expect("parse durable Raft state")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_minority_cannot_commit_and_healed_logs_converge() {
    let cluster = RunningCluster::start().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("partition test client");

    wait_until("node-a to win the deterministic initial leadership", || {
        cluster
            .node("node-a")
            .status()
            .is_ok_and(|status| status.role == Role::Leader)
    })
    .await
    .expect("initial leader");
    let initial_term = cluster
        .node("node-a")
        .status()
        .expect("initial leader status")
        .term;

    let baseline = client
        .put(format!("{}/v1/control/config", cluster.node_urls["node-a"]))
        .json(&routing("round-robin"))
        .send()
        .await
        .expect("write baseline configuration");
    assert_eq!(baseline.status(), reqwest::StatusCode::OK);
    wait_until("baseline revision 2 on every node", || {
        cluster.nodes.values().all(|node| {
            node.status().is_ok_and(|status| {
                status.commit_index >= 2
                    && status
                        .committed_configuration
                        .is_some_and(|configuration| configuration.revision == 2)
            })
        })
    })
    .await
    .expect("baseline convergence");

    let mut partition_forwarded = 0_u64;
    let mut partition_dropped = 0_u64;
    for (source, target) in [
        ("node-b", "node-a"),
        ("node-c", "node-a"),
        ("node-a", "node-b"),
        ("node-a", "node-c"),
    ] {
        let status = cluster
            .set_link_mode(
                &client,
                source,
                target,
                LinkMode::Drop,
                "isolate live old leader",
            )
            .await;
        assert_eq!(status.mode, LinkMode::Drop);
        assert_eq!(status.mode_changes, 1);
    }

    let old_write_client = client.clone();
    let old_leader_url = cluster.node_urls["node-a"].clone();
    let old_write = tokio::spawn(async move {
        old_write_client
            .put(format!("{old_leader_url}/v1/control/config"))
            .json(&routing("least-in-flight"))
            .send()
            .await
            .expect("isolated leader response")
    });
    wait_until("isolated leader to append its uncommitted suffix", || {
        cluster.node("node-a").status().is_ok_and(|status| {
            status.role == Role::Leader
                && status.commit_index == 2
                && status.last_log_index == 3
                && status.last_log_term == initial_term
        })
    })
    .await
    .expect("isolated suffix");

    wait_until("the connected majority to elect a replacement", || {
        ["node-b", "node-c"].into_iter().any(|node_id| {
            cluster
                .node(node_id)
                .status()
                .is_ok_and(|status| status.role == Role::Leader && status.term > initial_term)
        })
    })
    .await
    .expect("replacement leader");

    let old_response = old_write.await.expect("isolated write task");
    assert_eq!(
        old_response.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    let old_error: Value = old_response.json().await.expect("isolated write error");
    assert_eq!(old_error["error"]["code"], "unavailable");

    let isolated_state = read_state(&cluster.state_path("node-a"));
    assert_eq!(isolated_state["commit_index"], 2);
    assert_eq!(isolated_state["log"][2]["term"], initial_term);
    assert_eq!(
        isolated_state["log"][2]["command"]["configuration"]["routing_policy"],
        "least-in-flight"
    );

    let replacement_id = ["node-b", "node-c"]
        .into_iter()
        .find(|node_id| {
            cluster
                .node(node_id)
                .status()
                .is_ok_and(|status| status.role == Role::Leader)
        })
        .expect("majority leader");
    let majority_write = client
        .put(format!(
            "{}/v1/control/config",
            cluster.node_urls[replacement_id]
        ))
        .json(&routing("weighted-round-robin"))
        .send()
        .await
        .expect("majority write response");
    assert_eq!(majority_write.status(), reqwest::StatusCode::OK);
    let majority_body: Value = majority_write.json().await.expect("majority write body");
    assert_eq!(majority_body["revision"], 4);
    assert!(
        majority_body["term"]
            .as_u64()
            .is_some_and(|term| term > initial_term)
    );
    wait_until("majority revision 4 commit", || {
        ["node-b", "node-c"].into_iter().all(|node_id| {
            cluster.node(node_id).status().is_ok_and(|status| {
                status.commit_index >= 4
                    && status.committed_configuration.is_some_and(|configuration| {
                        configuration.revision == 4
                            && configuration.configuration.routing_policy == "weighted-round-robin"
                    })
            })
        })
    })
    .await
    .expect("majority progress");

    for (source, target) in [
        ("node-a", "node-b"),
        ("node-a", "node-c"),
        ("node-b", "node-a"),
        ("node-c", "node-a"),
    ] {
        let status = cluster
            .set_link_mode(
                &client,
                source,
                target,
                LinkMode::Allow,
                "heal isolated leader links",
            )
            .await;
        assert_eq!(status.mode, LinkMode::Allow);
        assert_eq!(status.mode_changes, 2);
        partition_forwarded = partition_forwarded.saturating_add(status.forwarded_requests);
        partition_dropped = partition_dropped.saturating_add(status.dropped_requests);
    }
    assert!(partition_forwarded > 0);
    assert!(partition_dropped > 0);

    wait_until(
        "old leader step-down and exact three-node convergence",
        || {
            let statuses = cluster
                .nodes
                .values()
                .filter_map(|node| node.status().ok())
                .collect::<Vec<NodeStatus>>();
            statuses.len() == 3
                && statuses
                    .iter()
                    .filter(|status| status.role == Role::Leader)
                    .count()
                    == 1
                && statuses.iter().all(|status| {
                    status.term > initial_term
                        && status.commit_index >= 4
                        && status.last_log_index >= 4
                        && status
                            .committed_configuration
                            .as_ref()
                            .is_some_and(|configuration| {
                                configuration.revision == 4
                                    && configuration.configuration.routing_policy
                                        == "weighted-round-robin"
                            })
                })
                && cluster
                    .node("node-a")
                    .status()
                    .is_ok_and(|status| status.role == Role::Follower)
        },
    )
    .await
    .expect("healed convergence");

    let states =
        ["node-a", "node-b", "node-c"].map(|node_id| read_state(&cluster.state_path(node_id)));
    assert_eq!(states[0]["log"], states[1]["log"]);
    assert_eq!(states[1]["log"], states[2]["log"]);
    assert_eq!(states[0]["commit_index"], states[1]["commit_index"]);
    assert_eq!(states[1]["commit_index"], states[2]["commit_index"]);
    assert_eq!(states[0]["log"][2]["term"], majority_body["term"]);
    assert_eq!(states[0]["log"][2]["command"]["type"], "noop");
    let converged_log = serde_json::to_string(&states[0]["log"]).expect("encode final log");
    assert!(!converged_log.contains("least-in-flight"));
    assert!(converged_log.contains("weighted-round-robin"));
}
