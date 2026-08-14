//! 事件扇出（模块 06）：WatchHub 统一广播 + SSE 流 + 集群 raft apply 转发。

use std::convert::Infallible;

use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use dsh_core::model::PublishEvent;
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

/// SSE 流：过滤 (project, branch) 的发布事件。
pub fn watch_sse(
    rx: tokio::sync::broadcast::Receiver<PublishEvent>,
    project: &str,
    branch: &str,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (p, b) = (project.to_string(), branch.to_string());
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |item| {
        let e: PublishEvent = item.ok()?;
        if e.project.as_str() == p.as_str() && e.branch.as_str() == b.as_str() {
            Some(Ok(
                SseEvent::default().data(serde_json::to_string(&e).unwrap_or_default())
            ))
        } else {
            None
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
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
}
