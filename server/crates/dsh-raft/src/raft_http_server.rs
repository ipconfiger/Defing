//! Raft RPC HTTP 服务端（axum，/raft/* 端点）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};

use crate::http_network::RaftHandle;
use crate::types::{NodeId, TypeConfig};

#[derive(Clone)]
pub struct RaftServerState {
    pub raft: RaftHandle,
}

/// 构建 Raft RPC 路由（挂到 raft_addr 或 http_addr 均可）。
pub fn raft_router(state: RaftServerState) -> Router {
    Router::new()
        .route("/raft/append-entries", post(append_entries))
        .route("/raft/vote", post(vote))
        .route("/raft/install-snapshot", post(install_snapshot))
        .with_state(state)
}

async fn append_entries(
    State(s): State<RaftServerState>,
    Json(rpc): Json<AppendEntriesRequest<TypeConfig>>,
) -> Result<Json<AppendEntriesResponse<NodeId>>, (StatusCode, String)> {
    s.raft
        .append_entries(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn vote(
    State(s): State<RaftServerState>,
    Json(rpc): Json<VoteRequest<NodeId>>,
) -> Result<Json<VoteResponse<NodeId>>, (StatusCode, String)> {
    s.raft
        .vote(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn install_snapshot(
    State(s): State<RaftServerState>,
    Json(rpc): Json<InstallSnapshotRequest<TypeConfig>>,
) -> Result<Json<InstallSnapshotResponse<NodeId>>, (StatusCode, String)> {
    s.raft
        .install_snapshot(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
