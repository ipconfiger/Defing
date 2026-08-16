//! 3 节点集群测试（M1 验收）：bootstrap → join → promote → 写读 → kill 容错。
//! 使用进程内直连网络（NetworkFactory）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use dsh_core::command::{Command, DraftUpdateItem};
use dsh_core::model::{BranchName, ItemDef, Value, ValueType};
use dsh_core::StateMachine;
use dsh_raft::*;
use dsh_storage::RedbStorage;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("dsh-cluster-{tag}-{}-{n}", std::process::id()))
}

struct NodeCtx {
    raft: RaftHandle,
    sm: Arc<RwLock<StateMachine>>,
    sm_store: Arc<StateMachineStore>,
    dir: std::path::PathBuf,
}

async fn make_node(id: NodeId, tag: &str, network: &NetworkFactory) -> NodeCtx {
    let dir = tmpdir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    let storage = RedbStorage::open(&dir.display().to_string()).unwrap();
    let db = storage.raw_db();
    let sm = Arc::new(RwLock::new(StateMachine::new(Box::new(storage))));
    let sm_store = Arc::new(StateMachineStore::new(sm.clone(), db.clone()));
    let log_store = LogStore::new(db.clone());
    let node = NodeInfo {
        grpc_addr: format!("127.0.0.1:{}", 8000 + id as u32),
        http_addr: format!("127.0.0.1:{}", 9000 + id as u32),
        raft_addr: format!("127.0.0.1:{}", 7000 + id as u32),
    };
    let raft = new_raft_node(
        id,
        node.clone(),
        log_store,
        sm_store.clone(),
        network,
        dev_config(),
    )
    .await
    .unwrap();
    network.register(id, raft.clone());
    NodeCtx {
        raft,
        sm,
        sm_store,
        dir,
    }
}

fn sm_has_project(sm: &RwLock<StateMachine>, name: &str) -> bool {
    let g = sm.read().unwrap();
    g.list_projects().unwrap().iter().any(|p| p.name == name)
}

fn sm_get_version(sm: &RwLock<StateMachine>, project: &str, branch: &str) -> Option<u64> {
    let g = sm.read().unwrap();
    g.get_config(&project.into(), &BranchName(branch.into()), 0)
        .ok()
        .map(|c| c.version)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_bootstrap_join_failover() {
    let network = NetworkFactory::new();

    // 1) bootstrap node1
    let n1 = make_node(1, "n1", &network).await;
    let node1 = NodeInfo {
        grpc_addr: "127.0.0.1:8001".into(),
        http_addr: "127.0.0.1:9001".into(),
        raft_addr: "127.0.0.1:7001".into(),
    };
    initialize_single(&n1.raft, 1, node1).await.unwrap();
    assert!(
        wait_for_leader(&n1.raft, Duration::from_secs(5))
            .await
            .is_some(),
        "node1 should become leader"
    );

    // 2) node2 join as learner → promote to voter
    let n2 = make_node(2, "n2", &network).await;
    let node2 = NodeInfo {
        grpc_addr: "127.0.0.1:8002".into(),
        http_addr: "127.0.0.1:9002".into(),
        raft_addr: "127.0.0.1:7002".into(),
    };
    let leader = network.get(&1).expect("node1 in peers");
    leader.add_learner(2, node2.clone(), false).await.unwrap();
    leader
        .change_membership(vec![1u64, 2], false)
        .await
        .unwrap();

    // 3) node3 join → promote
    let n3 = make_node(3, "n3", &network).await;
    let node3 = NodeInfo {
        grpc_addr: "127.0.0.1:8003".into(),
        http_addr: "127.0.0.1:9003".into(),
        raft_addr: "127.0.0.1:7003".into(),
    };
    leader.add_learner(3, node3.clone(), false).await.unwrap();
    leader
        .change_membership(vec![1u64, 2, 3], false)
        .await
        .unwrap();

    // 等集群稳定（出现 leader）
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut current = n1.raft.current_leader().await;
    if current.is_none() {
        current = n2.raft.current_leader().await;
    }
    assert!(current.is_some(), "cluster should elect a leader");

    // 4) 写：创建项目（经 leader，带重试）
    let leader_raft = network.get(&current.unwrap()).expect("leader raft");
    let resp = client_write(
        &leader_raft,
        Command::ProjectCreate {
            name: "order-service".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        resp.as_ref().unwrap().is_ok(),
        "project create failed: {resp:?}"
    );

    // 5) 复制：全部节点可见
    assert!(
        wait_until(
            || sm_has_project(&n1.sm, "order-service")
                && sm_has_project(&n2.sm, "order-service")
                && sm_has_project(&n3.sm, "order-service"),
            Duration::from_secs(5),
        )
        .await,
        "project should replicate to all nodes"
    );

    // 6) 结构 + 发布 + 值草稿 + 发布 → version 2 复制到所有节点
    let groups = vec![dsh_core::model::GroupDef {
        name: "redis".into(),
        items: vec![
            ItemDef {
                key: "host".into(),
                ty: ValueType::String,
                required: true,
                secret: false,
                validate: None,
            },
            ItemDef {
                key: "port".into(),
                ty: ValueType::Int,
                required: false,
                secret: false,
                validate: None,
            },
        ],
    }];
    client_write(
        &leader_raft,
        Command::StructureDraftSet {
            project: "order-service".into(),
            base_version: 1,
            groups: groups.clone(),

            operator: String::new(),
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();
    client_write(
        &leader_raft,
        Command::PublishStructure {
            project: "order-service".into(),
            comment: "init".into(),
            request_id: "s1".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();
    client_write(
        &leader_raft,
        Command::DraftUpdate {
            project: "order-service".into(),
            branch: BranchName("dev".into()),
            updates: vec![
                DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("127.0.0.1".into()),
                },
                DraftUpdateItem {
                    group: "redis".into(),
                    key: "port".into(),
                    value: Value::Int(6379),
                },
            ],
            deletes: vec![],

            operator: String::new(),
            ts: 0,
            expected_draft_rev: None,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();
    client_write(
        &leader_raft,
        Command::Publish {
            project: "order-service".into(),
            branch: BranchName("dev".into()),
            comment: "dev".into(),
            request_id: "r1".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        wait_until(
            || sm_get_version(&n3.sm, "order-service", "dev") == Some(2)
                && sm_get_version(&n1.sm, "order-service", "dev") == Some(2),
            Duration::from_secs(5),
        )
        .await,
        "version 2 should replicate (dev) to all nodes"
    );

    // 7) kill node2：多数派（1,3）仍可写
    network.remove(2);
    drop(n2.raft); // 关闭节点 2
    drop(n2.sm);
    let _ = std::fs::remove_dir_all(&n2.dir);

    tokio::time::sleep(Duration::from_millis(300)).await;
    // 新 leader 可能迁移：重新发现 leader
    let mut leader_id = wait_for_leader(&n1.raft, Duration::from_secs(5)).await;
    if leader_id.is_none() {
        leader_id = wait_for_leader(&n3.raft, Duration::from_secs(3)).await;
    }
    assert!(
        leader_id.is_some(),
        "cluster should keep a leader after kill"
    );
    let leader_raft = network.get(&leader_id.unwrap()).expect("leader raft");

    let resp = client_write(
        &leader_raft,
        Command::ProjectCreate {
            name: "svc-b".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        resp.as_ref().unwrap().is_ok(),
        "write after kill should succeed: {resp:?}"
    );

    // 8) 存活节点（1 或 3）可见新项目
    assert!(
        wait_until(
            || sm_has_project(&n3.sm, "svc-b") || sm_has_project(&n1.sm, "svc-b"),
            Duration::from_secs(5),
        )
        .await,
        "svc-b should be visible on surviving nodes"
    );

    // 清理
    drop(n1.raft);
    drop(n3.raft);
    let _ = std::fs::remove_dir_all(&n1.dir);
    let _ = std::fs::remove_dir_all(&n3.dir);
}

/// I7 会话跨节点唯一：login 经 leader 落库 → 复制到 follower → 跨节点二次 login 返回 ERR_SESSION_IN_USE。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_is_cluster_wide_single() {
    let network = NetworkFactory::new();
    let n1 = make_node(1, "s1", &network).await;
    let node1 = NodeInfo {
        grpc_addr: "127.0.0.1:8001".into(),
        http_addr: "127.0.0.1:9001".into(),
        raft_addr: "127.0.0.1:7001".into(),
    };
    initialize_single(&n1.raft, 1, node1).await.unwrap();
    let n2 = make_node(2, "s2", &network).await;
    let node2 = NodeInfo {
        grpc_addr: "127.0.0.1:8002".into(),
        http_addr: "127.0.0.1:9002".into(),
        raft_addr: "127.0.0.1:7002".into(),
    };
    let leader = network.get(&1).expect("node1 in peers");
    leader.add_learner(2, node2.clone(), false).await.unwrap();
    leader
        .change_membership(vec![1u64, 2], false)
        .await
        .unwrap();
    assert!(
        wait_for_leader(&n1.raft, Duration::from_secs(5))
            .await
            .is_some(),
        "node1 should be leader"
    );

    let token = "cluster-token-1";
    let hash = dsh_core::token_hash(token);
    // 1) 经 leader 登录（会话写入 Raft 日志）
    let r = client_write(
        &n1.raft,
        Command::SessionLogin {
            token_hash: hash.clone(),
            issued_at: 1000,
            expires_at: None,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(r.is_ok(), "first login should succeed: {r:?}");

    // 2) 复制到 follower：节点 B 本地状态机可见同一会话（跨节点唯一的前提）
    assert!(
        wait_until(
            || n2
                .sm
                .read()
                .unwrap()
                .get_session()
                .ok()
                .flatten()
                .map(|s| s.token_hash == hash)
                .unwrap_or(false),
            Duration::from_secs(5),
        )
        .await,
        "session should replicate to follower"
    );

    // 3) 跨节点二次登录 → ERR_SESSION_IN_USE（错误随 Raft 客户端响应返回）
    let r2 = client_write(
        &n1.raft,
        Command::SessionLogin {
            token_hash: dsh_core::token_hash("cluster-token-2"),
            issued_at: 2000,
            expires_at: None,
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        r2.unwrap_err().kind,
        dsh_core::ErrorKind::SessionInUse,
        "second login (even via leader) must be rejected"
    );

    drop(n1.raft);
    drop(n2.raft);
    let _ = std::fs::remove_dir_all(&n1.dir);
    let _ = std::fs::remove_dir_all(&n2.dir);
}

/// B7 集群 watch：follower 本地 apply 广播事件，订阅者（SSE 通道源头）能收到 leader 发布的发布事件。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_watch_events_reach_subscribers() {
    let network = NetworkFactory::new();
    let n1 = make_node(1, "w1", &network).await;
    let node1 = NodeInfo {
        grpc_addr: "127.0.0.1:8001".into(),
        http_addr: "127.0.0.1:9001".into(),
        raft_addr: "127.0.0.1:7001".into(),
    };
    initialize_single(&n1.raft, 1, node1).await.unwrap();
    let n2 = make_node(2, "w2", &network).await;
    let node2 = NodeInfo {
        grpc_addr: "127.0.0.1:8002".into(),
        http_addr: "127.0.0.1:9002".into(),
        raft_addr: "127.0.0.1:7002".into(),
    };
    let leader = network.get(&1).expect("node1 in peers");
    leader.add_learner(2, node2.clone(), false).await.unwrap();
    leader
        .change_membership(vec![1u64, 2], false)
        .await
        .unwrap();
    assert!(
        wait_for_leader(&n1.raft, Duration::from_secs(5))
            .await
            .is_some(),
        "node1 should be leader"
    );

    // follower（节点 2）订阅本节点 apply 事件（与 main.rs 集群 watch 通道同源）
    let mut rx = n2.sm_store.subscribe();

    // leader 写：建项目 → 结构草稿 → 结构发布（产生 3 分支事件）
    client_write(
        &n1.raft,
        Command::ProjectCreate {
            name: "watch-proj".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();
    let groups = vec![dsh_core::model::GroupDef {
        name: "redis".into(),
        items: vec![ItemDef {
            key: "host".into(),
            ty: ValueType::String,
            required: true,
            secret: false,
            validate: None,
        }],
    }];
    client_write(
        &n1.raft,
        Command::StructureDraftSet {
            project: "watch-proj".into(),
            base_version: 1,
            groups,

            operator: String::new(),
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();
    client_write(
        &n1.raft,
        Command::PublishStructure {
            project: "watch-proj".into(),
            comment: "init".into(),
            request_id: "ws1".into(),

            operator: String::new(),
            ts: 0,
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap()
    .unwrap();

    // follower 应收到 structure_publish 事件（3 分支；等待即可，事件随 apply 广播）
    let mut seen_structure_publish = false;
    for _ in 0..6 {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(e)) => {
                if e.ty == dsh_core::model::EventType::StructurePublish
                    && e.project.as_str() == "watch-proj"
                {
                    seen_structure_publish = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        seen_structure_publish,
        "follower subscriber should receive structure_publish event"
    );

    drop(n1.raft);
    drop(n2.raft);
    let _ = std::fs::remove_dir_all(&n1.dir);
    let _ = std::fs::remove_dir_all(&n2.dir);
}
