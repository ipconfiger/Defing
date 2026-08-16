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
    /// 可选 token：Some 时 /raft/* 端点要求 `Authorization: Bearer <token>` 完全相等；
    /// None 时保持无鉴权（默认行为，兼容现有部署）。
    pub token: Option<std::sync::Arc<str>>,
}

impl RaftServerState {
    pub fn new(raft: RaftHandle) -> Self {
        Self { raft, token: None }
    }

    pub fn with_token(raft: RaftHandle, token: Option<std::sync::Arc<str>>) -> Self {
        Self { raft, token }
    }
}

/// 统一鉴权：state.token 为 Some 时校验请求头 `Authorization: Bearer <token>` 完全相等，
/// 不匹配返回 401；token 为 None 时放行。
fn authed(
    state: &RaftServerState,
    req: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, String)> {
    if let Some(token) = &state.token {
        let expected = format!("Bearer {}", token);
        let ok = req
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == expected)
            .unwrap_or(false);
        if !ok {
            return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
        }
    }
    Ok(())
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
    headers: axum::http::HeaderMap,
    Json(rpc): Json<AppendEntriesRequest<TypeConfig>>,
) -> Result<Json<AppendEntriesResponse<NodeId>>, (StatusCode, String)> {
    authed(&s, &headers)?;
    s.raft
        .append_entries(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn vote(
    State(s): State<RaftServerState>,
    headers: axum::http::HeaderMap,
    Json(rpc): Json<VoteRequest<NodeId>>,
) -> Result<Json<VoteResponse<NodeId>>, (StatusCode, String)> {
    authed(&s, &headers)?;
    s.raft
        .vote(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn install_snapshot(
    State(s): State<RaftServerState>,
    headers: axum::http::HeaderMap,
    Json(rpc): Json<InstallSnapshotRequest<TypeConfig>>,
) -> Result<Json<InstallSnapshotResponse<NodeId>>, (StatusCode, String)> {
    authed(&s, &headers)?;
    s.raft
        .install_snapshot(rpc)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
