//! 事件扇出（模块 06）：WatchHub 统一广播 + SSE 流 + 集群 raft apply 转发。

use std::convert::Infallible;

use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use dsh_core::model::PublishEvent;
use dsh_core::wire::mask_event_for_wire;
use dsh_raft::StateMachineStore;
use futures::stream::Stream;
use tokio_stream::StreamExt as _;

/// 发布事件广播中心（dev-single 直发；集群由 raft apply 经 sm_store 转发）。
#[derive(Clone)]
pub struct WatchHub {
    tx: tokio::sync::broadcast::Sender<PublishEvent>,
}

impl Default for WatchHub {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchHub {
    pub fn new() -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self { tx }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PublishEvent> {
        self.tx.subscribe()
    }

    /// 底层广播 sender（供写路径直发）。
    pub fn sender(&self) -> &tokio::sync::broadcast::Sender<PublishEvent> {
        &self.tx
    }

    pub fn publish(&self, e: &PublishEvent) {
        let _ = self.tx.send(e.clone());
    }

    /// 集群 watch：把 raft apply 广播（sm_store.subscribe()）转发到本 hub（SSE 通道）。
    pub fn spawn_raft_forward(&self, sm_store: std::sync::Arc<StateMachineStore>) {
        let mut events_rx = sm_store.subscribe();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Ok(e) = events_rx.recv().await {
                let _ = tx.send(e);
            }
        });
    }
}

/// SSE 流（可测内部实现；[`watch_sse`] 仅包 Sse + keep_alive）：
/// 先重放 after_version 之后的历史事件（replay，由调用方按版本链合成），
/// 再订阅实时发布事件（版本号 > replay 末尾去重）。
/// - 慢消费者（广播缓冲溢出 Lagged）或通道关闭 → **流结束**（不再静默丢事件，F5）；
///   客户端应带 after_version 重连续传（design §6.2/§6.3）；
/// - `force_snapshot`（起点已被版本裁剪，D-PRUNED）：重放后补发一条
///   `snapshot_required: true` 事件并结束——客户端据此重拉全量，避免断线窗口静默丢事件。
///
/// 安全（F1）：所有出网事件经 `mask_event_for_wire` 掩码 secret 密文，重放与实时共用此唯一出口。
fn sse_stream(
    rx: tokio::sync::broadcast::Receiver<PublishEvent>,
    project: &str,
    branch: &str,
    replay: Vec<PublishEvent>,
    force_snapshot: bool,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    let (p, b) = (project.to_string(), branch.to_string());
    let last = replay.iter().map(|e| e.version).max().unwrap_or(0);
    let replay_iter = futures::stream::iter(replay.into_iter().map(|e| {
        Ok(SseEvent::default()
            .data(serde_json::to_string(&mask_event_for_wire(&e)).unwrap_or_default()))
    }));
    // 裁剪起点 → 补发 snapshot_required 合成事件（D-PRUNED）
    let snapshot_iter: futures::stream::BoxStream<'static, Result<SseEvent, Infallible>> =
        if force_snapshot {
            futures::stream::StreamExt::boxed(futures::stream::iter([Ok(SseEvent::default()
                .data(
                    serde_json::json!({
                        "project": p, "branch": b, "version": last,
                        "ty": "value_publish", "structure_version": 0,
                        "comment": "snapshot required", "request_id": "",
                        "changes": [], "snapshot_required": true,
                    })
                    .to_string(),
                ))]))
        } else {
            futures::stream::StreamExt::boxed(futures::stream::empty())
        };
    let live = tokio_stream::wrappers::BroadcastStream::new(rx)
        // 慢消费者（Err(Lagged)）/通道关闭（Err(Closed)）→ 结束流（F5，不再静默丢事件）
        .take_while(|item| item.is_ok())
        .filter_map(move |item| {
            let e: PublishEvent = item.ok()?;
            if e.project.as_str() == p.as_str()
                && e.branch.as_str() == b.as_str()
                // G3/D25 方案 b：gray 事件永不按版本过滤（promote/abort 补发不丢，Q4）；
                // `last` 为重放末尾固定值（非可变），无游标倒挂问题。
                && (e.gray || e.version > last)
            {
                Some(Ok(SseEvent::default().data(
                    serde_json::to_string(&mask_event_for_wire(&e)).unwrap_or_default(),
                )))
            } else {
                None
            }
        });
    // D-PRUNED：force_snapshot 语义 = 补发 snapshot_required 后**结束流**，
    // 客户端据此重拉全量并带新版本重新订阅（不接 live，避免客户端误以为仍连续）。
    if force_snapshot {
        futures::future::Either::Left(replay_iter.chain(snapshot_iter))
    } else {
        futures::future::Either::Right(replay_iter.chain(live))
    }
}

pub fn watch_sse(
    rx: tokio::sync::broadcast::Receiver<PublishEvent>,
    project: &str,
    branch: &str,
    replay: Vec<PublishEvent>,
    force_snapshot: bool,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    Sse::new(sse_stream(rx, project, branch, replay, force_snapshot))
        .keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::model::{BranchName, EventType, ProjectId};

    fn event(project: &str, branch: &str, version: u64) -> PublishEvent {
        PublishEvent {
            project: ProjectId(project.into()),
            branch: BranchName(branch.into()),
            version,
            ty: EventType::ValuePublish,
            structure_version: 1,
            comment: "c".into(),
            request_id: "r".into(),
            changes: vec![],
            gray: false,
        }
    }

    #[test]
    fn hub_broadcasts_to_subscribers() {
        let hub = WatchHub::new();
        let mut rx = hub.subscribe();
        hub.publish(&event("p", "dev", 2));
        let got = rx.try_recv().expect("event delivered");
        assert_eq!(got.version, 2);
        assert_eq!(got.project.as_str(), "p");
    }

    #[test]
    fn sender_is_shared() {
        let hub = WatchHub::new();
        let mut rx = hub.subscribe();
        let _ = hub.sender().send(event("p", "prod", 3));
        let got = rx.try_recv().expect("event delivered via sender");
        assert_eq!(got.version, 3);
    }

    /// D-TEST（F5）：慢消费者（广播缓冲溢出 → Lagged）→ 流结束而非静默丢事件。
    /// watch_sse 的实时段 take_while(is_ok)：首个 Err(Lagged) 即终止流。
    #[tokio::test]
    async fn slow_consumer_lagged_ends_stream() {
        let hub = WatchHub::new();
        // 先订阅（cursor 停在旧位置），再灌入超过 broadcast 容量（1024）的事件
        let rx = hub.subscribe();
        for i in 0..1100u64 {
            hub.publish(&event("p", "dev", i));
        }
        let mut s = sse_stream(rx, "p", "dev", vec![], false);
        // 首个 item 应为 None（Lagged → 流结束），而非继续输出事件
        let first = futures::stream::StreamExt::next(&mut s).await;
        assert!(
            first.is_none(),
            "慢消费者应结束流（got {first:?}），不得静默丢事件"
        );
    }

    /// D-TEST（D-PRUNED）：force_snapshot → 补发 snapshot_required 合成事件并结束。
    #[tokio::test]
    async fn pruned_start_emits_snapshot_required() {
        let hub = WatchHub::new();
        let rx = hub.subscribe();
        let mut s = sse_stream(rx, "p", "dev", vec![], true);
        // 首个元素应为 Ok(SseEvent)（合成 snapshot_required 事件）
        assert!(
            futures::stream::StreamExt::next(&mut s).await.is_some(),
            "force_snapshot 应补发合成事件"
        );
        // 补发后流结束
        assert!(
            futures::stream::StreamExt::next(&mut s).await.is_none(),
            "snapshot_required 后应结束流"
        );
    }

    /// G3/D25 方案 b：gray 事件永不按版本过滤——version ≤ last 的 gray 事件仍投递（Q4 补发）。
    /// 用 replay 末尾版本抬高 last：replay=[v10] → last=10；实时 gray 事件 v2（≤10）必须投递。
    #[tokio::test]
    async fn gray_event_bypasses_version_filter() {
        let hub = WatchHub::new();
        let rx = hub.subscribe();
        // replay 含 v10 → last=10；流先发 replay（v10）
        let mut s = sse_stream(rx, "p", "dev", vec![event("p", "dev", 10)], false);
        let first = futures::stream::StreamExt::next(&mut s).await;
        assert!(first.is_some(), "先重放 v10");

        // ① gray=true、version=2（≤ last=10）→ 必须投递（Q4：promote/abort 补发）
        let mut gray_ev = event("p", "dev", 2);
        gray_ev.gray = true;
        hub.publish(&gray_ev);
        let item = futures::stream::StreamExt::next(&mut s).await;
        assert!(item.is_some(), "gray 事件永不按版本过滤（D25/Q4）");

        // ② gray=false、version=5（≤ last=10）→ 不投递（filter_map 丢弃后继续等待 → 超时）
        hub.publish(&event("p", "dev", 5));
        let timeout = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            futures::stream::StreamExt::next(&mut s),
        )
        .await;
        assert!(timeout.is_err(), "普通低版本事件仍被过滤（应超时而非投递）");
    }
}
