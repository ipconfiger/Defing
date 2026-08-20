# 模块 06 —— 订阅与推送（dsh-watch）

> 依据：design-v2 §6、design-v3 §2.3/§2.4
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：订阅表、事件广播（leader 提交后全节点扇出）、断线重放、慢消费者、keepalive。
- 不做：事件产生（模块 04）；传输协议细节（gRPC 流由模块 05 暴露）。

## 2. 数据结构

```
pub struct Subscription {
    pub project: ProjectId, pub branch: BranchName,
    pub last_sent_version: u64,          // 断线续传锚点
    pub tx: mpsc::Sender<WatchEvent>,    // 每订阅缓冲 1000（溢出 → 慢消费者处理）
}
pub struct SubscriptionTable {
    map: DashMap<(ProjectId, BranchName), Vec<Subscription>>,
}
```

## 3. 事件流（发布 → 扇出）

```
// 模块 03 apply 返回 events → dsh-cli 组装层调用：
fn broadcast(events: Vec<PublishEvent>) {
    for e in events {
        let key = (e.project, e.branch);
        for sub in table.get(&key) {
            match sub.tx.try_send(e.clone()) {
                Ok(()) => sub.last_sent_version = e.version,
                Err(Full) => {
                    // 慢消费者：发 snapshot_required 事件并关闭（design-v3 §2.3）
                    sub.tx.try_send(WatchEvent { snapshot_required: true, .. });
                    close(sub);
                }
            }
        }
        metrics::watch_events_total.inc();
    }
}
```

## 4. 订阅与重放

```
async fn subscribe(req: WatchRequest) -> WatchStream {
    let mut sub = table.register(project, branch, req.after_version);
    // 1) 回当前版本号（若 after_version>0 且可重放 → 重放版本链事件）
    if req.after_version > 0 {
        let events = replay_after(project, branch, req.after_version)?;   // 模块 04 版本链
        for e in events { sub.tx.send(e).await?; }
    }
    // 2) 转实时：持续从 rx 取并转发（gRPC 流）
    while let Some(e) = rx.recv().await { yield e }
}
```

## 5. 重放边界
- 起点被裁剪：返回 ERR_VERSION_PRUNED（模块 05 映射 410）→ SDK 重拉全量。
- 事件日志保留：`--watch-event-retain`（默认 10k）与版本链双源；优先版本链。

## 6. keepalive
- 无事件 30s 发 keepalive（gRPC ping 或空事件）；客户端 60s 无数据判定断线。

## 7. 多节点扇出
- leader 提交后事件经内部 channel 广播到所有节点（或各节点订阅本地 apply 的复制事件——
  openraft 复制日志到所有节点，每个节点 apply 时本地扇出，天然一致）。

## 8. 测试要点
- WCH-001 断线重放不丢 ｜ WCH-002 慢消费者（snapshot_required+关流）｜ WCH-003 事件顺序严格递增

## 9. 任务清单
□ SubscriptionTable（注册/注销/关流） □ broadcast（含慢消费者） □ replay_after（版本链）
□ keepalive □ 事件顺序保证（版本单调校验） □ 指标（watch_conns/events/dropped）
□ 单元/集成测试 WCH-001~003
