# 模块 03 —— openraft 集成（dsh-raft）

> 依据：design-v2 §2、design-v3 §2.1
> 版本：v1.0 ｜ 状态：开发就绪（M1 固定 openraft 版本后核对 trait 签名）

## 1. 职责与边界
- 职责：openraft 实例组装（RaftTypeConfig/Storage/Network）、bootstrap、join/promote/remove、
  快照与追赶、选举参数、leader 重定向信息。
- 不做：状态机业务逻辑（apply 委托 dsh-core）；对外 API（dsh-api 提供 join 端点，本模块提供节点间 RPC client）。

## 2. 类型配置

```
pub type NodeId = u64;                       // 持久化 identity.json
pub struct Node { pub grpc_addr: String, pub http_addr: String, pub raft_addr: String }
pub struct RaftTypeConfig;
impl openraft::RaftTypeConfig for RaftTypeConfig {
    type D = Node;
    type R = openraft::raft::Entry<Command>;
    type NodeId = NodeId;
    type Node = Node;
}
```

## 3. 启动流程（dsh-cli 调用）

```
pub async fn start(config: NodeConfig) -> Result<(Raft, JoinHandle)> {
    let store = dsh_storage::open(&config.data_dir)?;          // 模块 02
    let state_machine = dsh_core::StateMachine::new(store);     // 模块 01
    let raft = Raft::new(config.node_id, config.raft_addr, log_store, sm_store, raft_config).await?;
    if config.bootstrap { raft.initialize(vec![(node_id, node)]).await?; }
    else if let Some(join) = &config.join { join_cluster(&raft, join).await?; }
    Ok((raft, spawn_background(raft.clone())))
}
```

## 4. join / promote / remove（对应 design-v3 §2.1）

| 操作 | 入口 | 实现 |
|------|------|------|
| join | POST /api/v1/cluster/join（任意节点） | 转发 leader → add_learner(node_id, node) → 返回成员表 |
| promote | POST /api/v1/cluster/promote | change_membership([voters], false) |
| remove | POST /api/v1/cluster/remove | 先 change_membership 降级再移除 learner |
| 成员表 | ListMembers（gRPC）+ GET /api/v1/cluster/members | 从 raft.metrics() 派生 |

## 5. 状态机存储（RaftStateMachine）

```
pub struct StateMachineStore {
    pub sm: StateMachine,          // dsh-core
    pub storage: Arc<dyn Storage>, // 模块 02
}
// 实现 openraft::RaftStateMachine：
//   - apply(entry)：entry.data 反序列化为 Command → core.apply(cmd, now_ms)
//     → 返回事件（由 dsh-watch 消费，模块 06）
//   - get_snapshot / begin_receiving_snapshot / install_snapshot：经 rocksdb checkpoint
```

## 6. 快照与追赶
- 触发：--snapshot-interval（10k 条）/ --snapshot-size（64MB）。
- 追赶限速：--snapshot-limit（64MB/s）；RaftNetwork 走 gRPC 流。

## 7. 选举与读
- 参数：heartbeat 500ms；election timeout 1500~3000ms；--read-mode（linear/stale）：
  - linear：读走 ReadIndex（ensure_linearizable）后本地读；写走 leader。
  - stale：follower 本地读（可能稍旧但已提交）。
- leader 重定向：dsh-api 捕获 ForwardToLeader → ERR_LEADER_REDIRECT{leader_hint}。

## 8. 错误处理与运维
- 错误：ErrorKind::Raft（选举失败/成员变更冲突），CLI 输出可读信息。
- 运维：dsh admin promote / remove-node / snapshot 映射到 §4 操作。

## 9. 测试要点（对应 design-v3 §5）
- RAFT-001/002/003/004：3 节点 kill/分区/join 追平/落后追赶——用 dsh-testkit 的
  TestCluster（先进程内多 Raft，后进程级）。
- 成员变更期间脑裂防护验证（openraft 保证，测试确认）。

## 10. 任务清单
□ 固定 openraft 版本并核对 trait □ NodeId/Node/RaftTypeConfig
□ StateMachineStore（apply/快照） □ RaftNetwork（gRPC）
□ bootstrap/join/promote/remove □ 快照追赶限速 □ ReadIndex 线性读
□ 错误映射（ForwardToLeader） □ TestCluster 骨架（testkit） □ 集群测试 RAFT-001~004
