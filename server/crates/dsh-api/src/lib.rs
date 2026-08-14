//! HTTP 管理面 + 数据面（模块 05 / 09）：axum 路由、鉴权中间件、错误映射、Admin UI。
//! 职责：请求/响应编解码、会话校验（I7）、写路径（经 dsh-raft::write_command）、
//! 渲染端点（模块 08）、数据面快照/watch（模块 06）。
//! 不做：状态机业务（dsh-core/dsh-publish）、事件扇出（dsh-watch）、可观测（dsh-observability）。

pub mod grpc;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use dsh_core::command::{Command, DraftUpdateItem};
use dsh_core::model::{BranchName, GroupDef, ProjectId, RefBinding, SharedItem, Value, ValueType};
use dsh_core::{ErrorKind, StateMachine};
use dsh_crypto::Cipher;
use dsh_observability::{cluster_members_json, is_ready, metrics_text, AuditLog};
use dsh_publish::PublishService;
use dsh_raft::{NodeInfo as RaftNodeInfo, RaftHandle};
use dsh_render::{Format, Renderer};
use dsh_watch::{watch_sse, WatchHub};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

// ---------------- 状态 ----------------

/// HTTP 服务共享状态（axum State）。
#[derive(Clone)]
pub struct ApiState {
    pub sm: Arc<Mutex<StateMachine>>,
    /// 发布事件广播（watch SSE；dev-single 直发，集群由 raft apply 转发）
    pub hub: WatchHub,
    /// 集群模式下的 Raft 句柄
    pub raft: Option<RaftHandle>,
    pub node_id: Option<u64>,
    /// 加密器（secret 项加密/解密；未配置主密钥时 secret 写入被拒）
    pub cipher: Option<Arc<Cipher>>,
    /// 会话 TTL（I7）
    pub session_ttl: std::time::Duration,
    /// 管理员密码（--admin-password 或首启生成打印）
    pub admin_password: Arc<str>,
    /// 发布编排（模块 04）
    pub publish: PublishService,
    /// 审计落库（模块 10）
    pub audit: AuditLog,
    /// 主密钥环文件路径（{master-key-file}.ring.json；轮换后持久化，重启可加载）
    pub ring_path: Option<std::path::PathBuf>,
    /// 版本保留数（0=全量保留；admin/retention-status 展示）
    pub version_retention: u64,
    /// 审计保留条数（0=不裁剪）
    pub audit_retention: u64,
}

impl ApiState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sm: Arc<Mutex<StateMachine>>,
        hub: WatchHub,
        raft: Option<RaftHandle>,
        node_id: Option<u64>,
        cipher: Option<Arc<Cipher>>,
        session_ttl: std::time::Duration,
        admin_password: Arc<str>,
        ring_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self::with_retention(
            sm,
            hub,
            raft,
            node_id,
            cipher,
            session_ttl,
            admin_password,
            ring_path,
            0,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_retention(
        sm: Arc<Mutex<StateMachine>>,
        hub: WatchHub,
        raft: Option<RaftHandle>,
        node_id: Option<u64>,
        cipher: Option<Arc<Cipher>>,
        session_ttl: std::time::Duration,
        admin_password: Arc<str>,
        ring_path: Option<std::path::PathBuf>,
        version_retention: u64,
        audit_retention: u64,
    ) -> Self {
        let publish = PublishService::new(
            sm.clone(),
            cipher.clone(),
            raft.clone(),
            Some(hub.sender().clone()),
        );
        let audit = AuditLog::new(sm.clone(), raft.clone());
        Self {
            sm,
            hub,
            raft,
            node_id,
            cipher,
            session_ttl,
            admin_password,
            publish,
            audit,
            ring_path,
            version_retention,
            audit_retention,
        }
    }

    /// 通用写（dev-single 直 apply；集群经 Raft client_write）。
    async fn write(&self, cmd: &Command, now_ms: i64) -> Result<dsh_raft::WriteOutcome, ApiError> {
        dsh_raft::write_command(
            &self.sm,
            self.raft.as_ref(),
            cmd,
            now_ms,
            Some(self.hub.sender()),
        )
        .await
        .map_err(ApiError::from)
    }
}

// ---------------- 错误 ----------------

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

pub struct ApiError(pub dsh_core::Error);

impl From<dsh_core::Error> for ApiError {
    fn from(e: dsh_core::Error) -> Self {
        Self(e)
    }
}

impl From<ApiError> for (StatusCode, Json<ApiErrorBody>) {
    fn from(e: ApiError) -> Self {
        let status = match e.0.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict | ErrorKind::NoDraft | ErrorKind::SessionInUse => {
                StatusCode::CONFLICT
            }
            ErrorKind::SessionExpired => StatusCode::UNAUTHORIZED,
            ErrorKind::Forbidden => StatusCode::FORBIDDEN,
            ErrorKind::Validation | ErrorKind::PublishBlocked | ErrorKind::CycleRef => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            ErrorKind::LimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::LeaderRedirect => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ApiErrorBody {
                code: e.0.kind.code().into(),
                message: e.0.message,
                detail: e.0.detail.or_else(|| {
                    e.0.leader_hint
                        .map(|h| serde_json::json!({ "leader_hint": h }))
                }),
            }),
        )
    }
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiErrorBody>)>;

// ---------------- 请求/响应体 ----------------

#[derive(Deserialize)]
struct CreateProjectReq {
    name: String,
}

#[derive(Deserialize)]
struct CreateBranchReq {
    name: String,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
struct StructureDraftReq {
    base_version: u64,
    groups: Vec<GroupDef>,
}

#[derive(Deserialize)]
struct PublishReq {
    comment: String,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct RollbackReq {
    to_version: u64,
    comment: String,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct DraftUpdateReq {
    #[serde(default)]
    updates: Vec<DraftUpdateItem>,
    /// "group/key" 列表
    #[serde(default)]
    deletes: Vec<String>,
}

#[derive(Deserialize)]
struct JoinReq {
    node_id: u64,
    /// 仅 cluster/join 需要；promote 可缺省
    #[serde(default)]
    http_addr: String,
    #[serde(default)]
    raft_addr: String,
}

#[derive(Serialize)]
struct ConfigResp {
    project: String,
    branch: String,
    version: u64,
    structure_version: u64,
    groups: serde_json::Value,
}

#[derive(Serialize)]
struct ErrorOk {
    status: &'static str,
}

// ---------------- handlers ----------------

async fn health() -> Json<ErrorOk> {
    Json(ErrorOk { status: "ok" })
}

// ---------------- Admin UI（模块 09：内嵌静态页） ----------------

#[derive(rust_embed::Embed)]
#[folder = "admin/"]
struct AdminAssets;

async fn admin_index() -> axum::response::Response {
    match AdminAssets::get("index.html") {
        Some(f) => axum::response::Response::builder()
            .header("content-type", "text/html; charset=utf-8")
            .body(axum::body::Body::from(f.data.into_owned()))
            .expect("admin index"),
        None => axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::empty())
            .expect("404"),
    }
}

async fn admin_static(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> axum::response::Response {
    match AdminAssets::get(&path) {
        Some(f) => axum::response::Response::builder()
            .header("content-type", "application/octet-stream")
            .body(axum::body::Body::from(f.data.into_owned()))
            .expect("asset"),
        None => axum::response::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::empty())
            .expect("404"),
    }
}

/// 安全响应头（M4 加固）。
async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "x-frame-options",
        axum::http::HeaderValue::from_static("DENY"),
    );
    h.insert(
        "content-security-policy",
        axum::http::HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'",
        ),
    );
    resp
}

/// 管理面鉴权中间件（单管理员会话；除 login/healthz/readyz 外均需 Bearer）。
async fn auth_middleware(
    State(app): State<ApiState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorBody>)> {
    let path = req.uri().path().to_string();
    // cluster/join 是节点加入前的引导调用（尚无管理员会话），豁免鉴权
    if path == "/api/v1/login"
        || path == "/healthz"
        || path == "/readyz"
        || path == "/api/v1/cluster/join"
    {
        return Ok(next.run(req).await);
    }
    if path.starts_with("/api/v1/") {
        let auth = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.to_string());
        let ok = match auth {
            Some(t) => {
                // 校验状态机会话（跨节点唯一，I7）：token 哈希匹配 + 未过期
                let hash = dsh_core::token_hash(&t);
                let sm = app.sm.lock().expect("sm lock");
                match sm.get_session().ok().flatten() {
                    Some(s) => {
                        let hash_ok = s.token_hash == hash;
                        let not_expired = s.expires_at.map(|e| now_ms() < e).unwrap_or(true);
                        hash_ok && not_expired
                    }
                    None => false,
                }
            }
            None => false,
        };
        if !ok {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ApiErrorBody {
                    code: "ERR_SESSION_EXPIRED".into(),
                    message: "需要管理员会话".into(),
                    detail: None,
                }),
            ));
        }
    }
    Ok(next.run(req).await)
}

async fn create_project(
    State(app): State<ApiState>,
    Json(req): Json<CreateProjectReq>,
) -> ApiResult<serde_json::Value> {
    let pid = ProjectId(req.name.clone());
    app.write(&Command::ProjectCreate { name: req.name }, now_ms())
        .await?;
    app.audit
        .append(
            "project_create",
            Some(pid.as_str().into()),
            None,
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "id": pid.as_str(), "branches": ["dev", "test", "prod"] }),
    ))
}

async fn list_projects(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let projects = sm
        .list_projects()
        .map_err(ApiError::from)?
        .into_iter()
        .map(|p| serde_json::json!({ "id": p.id.as_str(), "name": p.name }))
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!(projects)))
}

async fn list_branches(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let id = ProjectId(pid);
    let branches = sm
        .list_branches(&id)
        .map_err(ApiError::from)?
        .into_iter()
        .map(|b| {
            let st = sm.get_branch_state(&id, &b).ok().flatten();
            serde_json::json!({
                "name": b.as_str(),
                "active_version": st.map(|s| s.active_version).unwrap_or(0)
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!(branches)))
}

async fn create_branch(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<CreateBranchReq>,
) -> ApiResult<serde_json::Value> {
    let source = req.source.as_deref().map(BranchName::from);
    app.write(
        &Command::BranchCreate {
            project: ProjectId(pid.clone()),
            name: BranchName(req.name.clone()),
            source,
        },
        now_ms(),
    )
    .await?;
    app.audit
        .append(
            "branch_create",
            Some(pid.clone()),
            Some(req.name.clone()),
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "name": req.name, "project": pid }),
    ))
}

async fn get_structure_draft(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let draft = sm
        .get_structure_draft(&ProjectId(pid))
        .map_err(ApiError::from)?;
    match draft {
        Some(d) => Ok(Json(serde_json::to_value(d).expect("serialize"))),
        None => Ok(Json(
            serde_json::json!({ "base_version": null, "groups": [] }),
        )),
    }
}

async fn set_structure_draft(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<StructureDraftReq>,
) -> ApiResult<serde_json::Value> {
    app.write(
        &Command::StructureDraftSet {
            project: ProjectId(pid.clone()),
            base_version: req.base_version,
            groups: req.groups,
        },
        now_ms(),
    )
    .await?;
    app.audit
        .append(
            "draft_update",
            Some(pid.clone()),
            None,
            None,
            None,
            serde_json::json!({ "kind": "structure-draft" }),
        )
        .await;
    Ok(Json(serde_json::json!({ "saved": true, "project": pid })))
}

async fn publish_structure(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<PublishReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let outcome = app
        .publish
        .publish_structure(&ProjectId(pid.clone()), &req.comment, &rid)
        .await
        .map_err(ApiError::from)?;
    let affected = outcome
        .affected
        .iter()
        .map(|(b, v)| serde_json::json!({ "branch": b, "version": v }))
        .collect::<Vec<_>>();
    app.audit
        .append(
            "structure_publish",
            Some(pid.clone()),
            None,
            None,
            Some(rid.clone()),
            serde_json::json!({ "affected_branches": affected }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "affected_branches": affected, "request_id": rid }),
    ))
}

async fn update_draft(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    Json(req): Json<DraftUpdateReq>,
) -> ApiResult<serde_json::Value> {
    let deletes: Vec<(String, String)> = req
        .deletes
        .iter()
        .filter_map(|s| {
            s.split_once('/')
                .map(|(g, k)| (g.to_string(), k.to_string()))
        })
        .collect();
    let updates_len = req.updates.len();
    let deletes_len = deletes.len();
    let pid_obj = ProjectId(pid.clone());
    let branch_obj = BranchName(branch.clone());
    app.publish
        .update_draft(&pid_obj, &branch_obj, req.updates, deletes)
        .await
        .map_err(ApiError::from)?;
    app.audit
        .append(
            "draft_update",
            Some(pid.clone()),
            Some(branch.clone()),
            None,
            None,
            serde_json::json!({ "updates": updates_len, "deletes": deletes_len }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "saved": true, "project": pid, "branch": branch }),
    ))
}

async fn publish(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    Json(req): Json<PublishReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let outcome = app
        .publish
        .publish(
            &ProjectId(pid.clone()),
            &BranchName(branch.clone()),
            &req.comment,
            &rid,
        )
        .await
        .map_err(ApiError::from)?;
    app.audit
        .append(
            "publish",
            Some(pid.clone()),
            Some(branch.clone()),
            Some(outcome.version),
            Some(rid.clone()),
            serde_json::json!({}),
        )
        .await;
    Ok(Json(serde_json::json!({
        "version": outcome.version,
        "changes": serde_json::to_value(&outcome.changes).expect("serialize"),
        "request_id": rid,
    })))
}

async fn rollback(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    Json(req): Json<RollbackReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let new_version = app
        .publish
        .rollback(
            &ProjectId(pid.clone()),
            &BranchName(branch.clone()),
            req.to_version,
            &req.comment,
            &rid,
        )
        .await
        .map_err(ApiError::from)?;
    app.audit
        .append(
            "rollback",
            Some(pid.clone()),
            Some(branch.clone()),
            Some(new_version),
            Some(rid.clone()),
            serde_json::json!({ "to_version": req.to_version }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "new_version": new_version, "request_id": rid }),
    ))
}

// ---------------- 项目/分支详情与删除（openapi 契约补全） ----------------

async fn project_detail(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let p = sm
        .get_project(&ProjectId(pid.clone()))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError(dsh_core::Error::not_found("project")))?;
    Ok(Json(serde_json::json!({
        "id": p.id.as_str(),
        "name": p.name,
        "created_at": p.created_at,
    })))
}

#[derive(Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_project(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<ForceQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    if !q.force {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiErrorBody {
                code: "ERR_VALIDATION".into(),
                message: "删除项目需要 force=true 确认".into(),
                detail: None,
            }),
        ));
    }
    let pid_obj = ProjectId(pid.clone());
    app.write(&Command::ProjectDelete { id: pid_obj }, now_ms())
        .await
        .map_err(Into::<(StatusCode, Json<ApiErrorBody>)>::into)?;
    app.audit
        .append(
            "project_delete",
            Some(pid.clone()),
            None,
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn branch_detail(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let id = ProjectId(pid.clone());
    let bname = BranchName(branch.clone());
    let st = sm
        .get_branch_state(&id, &bname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError(dsh_core::Error::not_found("branch")))?;
    let drafts: serde_json::Value = st
        .value_draft
        .iter()
        .map(|(g, items)| {
            let m: serde_json::Map<String, serde_json::Value> = items
                .iter()
                .map(|(k, dv)| {
                    (
                        k.clone(),
                        serde_json::json!({ "value": dv.value, "updated_at": dv.updated_at }),
                    )
                })
                .collect();
            (g.clone(), serde_json::Value::Object(m))
        })
        .collect();
    Ok(Json(serde_json::json!({
        "name": branch,
        "active_version": st.active_version,
        "structure_version": st.structure_version,
        "draft": drafts,
    })))
}

async fn delete_branch(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    app.write(
        &Command::BranchDelete {
            project: ProjectId(pid.clone()),
            name: BranchName(branch.clone()),
        },
        now_ms(),
    )
    .await
    .map_err(Into::<(StatusCode, Json<ApiErrorBody>)>::into)?;
    app.audit
        .append(
            "branch_delete",
            Some(pid.clone()),
            Some(branch.clone()),
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------- 分支对比 + 值提升（openapi 契约补全） ----------------

#[derive(Deserialize)]
struct DiffQuery {
    branch_a: String,
    branch_b: String,
}

async fn branch_diff(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<DiffQuery>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let id = ProjectId(pid);
    let a = sm
        .get_config(&id, &BranchName(q.branch_a), 0)
        .map_err(ApiError::from)?;
    let b = sm
        .get_config(&id, &BranchName(q.branch_b), 0)
        .map_err(ApiError::from)?;
    let mut keys: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for (g, items) in &a.groups {
        for k in items.keys() {
            keys.insert((g.clone(), k.clone()));
        }
    }
    for (g, items) in &b.groups {
        for k in items.keys() {
            keys.insert((g.clone(), k.clone()));
        }
    }
    let mut diffs = Vec::new();
    let mut missing = Vec::new();
    for (g, k) in keys {
        let va = a.groups.get(&g).and_then(|m| m.get(&k));
        let vb = b.groups.get(&g).and_then(|m| m.get(&k));
        match (va, vb) {
            (Some(x), Some(y)) if x != y => {
                diffs.push(serde_json::json!({
                    "group": g, "key": k, "branch_a": x, "branch_b": y,
                }));
            }
            (Some(_), None) | (None, Some(_)) => missing.push(format!("{g}/{k}")),
            _ => {}
        }
    }
    Ok(Json(
        serde_json::json!({ "diffs": diffs, "missing": missing }),
    ))
}

#[derive(Deserialize)]
struct PromoteReq {
    from: String,
    to: String,
    /// 限定 item（"group/key"）；缺省=全部
    items: Option<Vec<String>>,
    #[serde(default)]
    force: bool,
}

/// 值提升（design-v2 §4.8）：把源分支活动版本值写入目标分支草稿（不发布）。
/// 目标草稿已修改项默认跳过（force=true 覆盖）；items 中源分支没有的进入 missing_from。
async fn promote(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<PromoteReq>,
) -> ApiResult<serde_json::Value> {
    let pid_obj = ProjectId(pid.clone());
    let from_b = BranchName(req.from.clone());
    let to_b = BranchName(req.to.clone());
    let filter: Option<Vec<(String, String)>> = req.items.as_ref().map(|items| {
        items
            .iter()
            .filter_map(|s| {
                s.split_once('/')
                    .map(|(g, k)| (g.to_string(), k.to_string()))
            })
            .collect()
    });
    let (updates, applied, skipped, missing_from) = {
        let sm = app.sm.lock().expect("sm lock");
        let src = sm
            .get_config(&pid_obj, &from_b, 0)
            .map_err(ApiError::from)?;
        let dst = sm
            .get_branch_state(&pid_obj, &to_b)
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError(dsh_core::Error::not_found("target branch")))?;
        let mut updates = Vec::new();
        let mut applied = Vec::new();
        let mut skipped = Vec::new();
        let mut missing_from = Vec::new();
        for (g, items) in &src.groups {
            for (k, v) in items {
                if let Some(f) = &filter {
                    if !f.contains(&(g.clone(), k.clone())) {
                        continue;
                    }
                }
                let key = format!("{g}/{k}");
                let draft_modified = dst.value_draft.get(g).is_some_and(|m| m.contains_key(k));
                if draft_modified && !req.force {
                    skipped.push(key);
                    continue;
                }
                updates.push(DraftUpdateItem {
                    group: g.clone(),
                    key: k.clone(),
                    value: v.clone(),
                });
                applied.push(key);
            }
        }
        if let Some(f) = &filter {
            for (g, k) in f {
                if !src.groups.get(g).is_some_and(|m| m.contains_key(k)) {
                    missing_from.push(format!("{g}/{k}"));
                }
            }
        }
        (updates, applied, skipped, missing_from)
    };
    if !updates.is_empty() {
        app.publish
            .update_draft(&pid_obj, &to_b, updates, vec![])
            .await
            .map_err(ApiError::from)?;
    }
    app.audit
        .append(
            "promote",
            Some(pid.clone()),
            Some(req.to.clone()),
            None,
            None,
            serde_json::json!({
                "from": req.from,
                "applied": applied.len(),
                "skipped": skipped.len(),
                "missing_from": missing_from.len(),
            }),
        )
        .await;
    Ok(Json(serde_json::json!({
        "applied": applied,
        "skipped": skipped,
        "missing_from": missing_from,
    })))
}

// ---------------- 共享库（openapi 契约补全；core 已支持，补 HTTP 面） ----------------

/// 共享项请求（对齐 openapi SharedItem；version 由状态机分配）。
#[derive(Deserialize)]
struct SharedItemReq {
    group: String,
    key: String,
    r#type: ValueType,
    #[serde(default)]
    secret: bool,
    #[serde(default)]
    required: bool,
    value: Value,
}

fn masked_shared_value(item: &SharedItem) -> serde_json::Value {
    if item.secret {
        serde_json::json!({ "type": "string", "str_value": "***", "masked": true })
    } else {
        serde_json::json!(item.value)
    }
}

fn shared_item_json(item: &SharedItem) -> serde_json::Value {
    serde_json::json!({
        "group": item.group,
        "key": item.key,
        "type": item.ty,
        "secret": item.secret,
        "required": item.required,
        "value": masked_shared_value(item),
        "version": item.version,
    })
}

/// 写共享草稿（secret 项提交前加密，I8）。
async fn write_shared_draft(
    app: &ApiState,
    req: SharedItemReq,
    action: &str,
) -> Result<serde_json::Value, (StatusCode, Json<ApiErrorBody>)> {
    let mut value = req.value;
    if req.secret {
        if let Value::String(plain) = &value {
            let cipher = app.cipher.as_ref().ok_or_else(|| {
                ApiError(dsh_core::Error::validation(
                    "secret 共享项需要主密钥（--master-key-file 或 DSH_MASTER_KEY）",
                ))
            })?;
            let ct = cipher
                .encrypt_secret(plain.as_bytes())
                .map_err(|e| ApiError(dsh_core::Error::internal(format!("encrypt: {e}"))))?;
            value = Value::Secret(ct);
        }
    }
    let item = SharedItem {
        group: req.group.clone(),
        key: req.key.clone(),
        ty: req.r#type,
        secret: req.secret,
        required: req.required,
        value,
        version: 0,
    };
    app.write(&Command::SharedDraftUpdate { item }, now_ms())
        .await
        .map_err(Into::<(StatusCode, Json<ApiErrorBody>)>::into)?;
    app.audit
        .append(
            action,
            None,
            None,
            None,
            None,
            serde_json::json!({ "group": req.group, "key": req.key }),
        )
        .await;
    Ok(serde_json::json!({
        "saved": true,
        "group": req.group,
        "key": req.key,
    }))
}

async fn create_shared(
    State(app): State<ApiState>,
    Json(req): Json<SharedItemReq>,
) -> ApiResult<serde_json::Value> {
    write_shared_draft(&app, req, "shared_draft_update")
        .await
        .map(Json)
}

async fn update_shared_draft(
    State(app): State<ApiState>,
    Json(req): Json<SharedItemReq>,
) -> ApiResult<serde_json::Value> {
    write_shared_draft(&app, req, "shared_draft_update")
        .await
        .map(Json)
}

async fn list_shared(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let items = sm
        .list_shared_published()
        .map_err(ApiError::from)?
        .iter()
        .map(shared_item_json)
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!(items)))
}

async fn list_shared_drafts(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let items = sm
        .list_shared_drafts()
        .map_err(ApiError::from)?
        .iter()
        .map(shared_item_json)
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!(items)))
}

async fn publish_shared(
    State(app): State<ApiState>,
    Json(req): Json<PublishReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let outcome = app
        .write(
            &Command::SharedPublish {
                comment: req.comment,
                request_id: rid.clone(),
            },
            now_ms(),
        )
        .await?;
    let mut affected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in &outcome.events {
        let k = (
            e.project.as_str().to_string(),
            e.branch.as_str().to_string(),
        );
        if seen.insert(k) {
            affected.push(serde_json::json!({
                "project": e.project.as_str(),
                "branch": e.branch.as_str(),
                "new_version": e.version,
            }));
        }
    }
    let max_version = {
        let sm = app.sm.lock().expect("sm lock");
        sm.list_shared_published()
            .map_err(ApiError::from)?
            .iter()
            .map(|i| i.version)
            .max()
            .unwrap_or(0)
    };
    app.audit
        .append(
            "shared_publish",
            None,
            None,
            None,
            Some(rid.clone()),
            serde_json::json!({ "affected": affected.len() }),
        )
        .await;
    Ok(Json(serde_json::json!({
        "version": max_version,
        "affected": affected,
        "request_id": rid,
    })))
}

// ---------------- 共享引用绑定（core RefBind/RefUnbind 补 HTTP 面） ----------------

#[derive(Deserialize)]
struct RefBindReq {
    project: String,
    group: String,
    #[serde(default)]
    item_key: Option<String>,
    shared_group: String,
    shared_key: String,
}

#[derive(Deserialize)]
struct RefUnbindReq {
    project: String,
    group: String,
    #[serde(default)]
    item_key: Option<String>,
}

async fn ref_bind(
    State(app): State<ApiState>,
    Json(req): Json<RefBindReq>,
) -> ApiResult<serde_json::Value> {
    let binding = RefBinding {
        group: req.group.clone(),
        item_key: req.item_key.clone(),
        shared_group: req.shared_group.clone(),
        shared_key: req.shared_key.clone(),
    };
    app.write(
        &Command::RefBind {
            project: ProjectId(req.project.clone()),
            binding,
        },
        now_ms(),
    )
    .await?;
    app.audit
        .append(
            "ref_bind",
            Some(req.project.clone()),
            None,
            None,
            None,
            serde_json::json!({ "group": req.group, "item_key": req.item_key, "shared": format!("{}/{}", req.shared_group, req.shared_key) }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "bound": true, "project": req.project }),
    ))
}

async fn ref_unbind(
    State(app): State<ApiState>,
    Json(req): Json<RefUnbindReq>,
) -> ApiResult<serde_json::Value> {
    app.write(
        &Command::RefUnbind {
            project: ProjectId(req.project.clone()),
            group: req.group.clone(),
            item_key: req.item_key.clone(),
        },
        now_ms(),
    )
    .await?;
    app.audit
        .append(
            "ref_unbind",
            Some(req.project.clone()),
            None,
            None,
            None,
            serde_json::json!({ "group": req.group, "item_key": req.item_key }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "unbound": true, "project": req.project }),
    ))
}

#[derive(Deserialize)]
struct RefsQuery {
    project: String,
}

async fn list_refs(
    State(app): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<RefsQuery>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let refs = sm
        .list_refs(&ProjectId(q.project))
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!(refs)))
}

/// secret 值处理策略（§7.6）：reveal=false 一律掩码；reveal=true 解密（需会话 + 审计）。
/// 数据面（HTTP snapshot / gRPC）与渲染/导出默认掩码 —— 与 proto masked 语义及 gRPC 行为一致。
fn apply_secret_policy(
    groups: &mut std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>>,
    cipher: Option<&Cipher>,
    reveal: bool,
) {
    for items in groups.values_mut() {
        for v in items.values_mut() {
            if let Value::Secret(ct) = v {
                *v = if reveal {
                    match cipher.and_then(|c| c.decrypt_secret(ct).ok()) {
                        Some(p) => Value::String(String::from_utf8_lossy(&p).into_owned()),
                        None => Value::String("***".into()),
                    }
                } else {
                    Value::String("***".into())
                };
            }
        }
    }
}

#[derive(Deserialize)]
struct ConfigQuery {
    #[serde(default)]
    reveal: bool,
}

/// 管理面查看配置（GET /api/v1/projects/{p}/branches/{b}/config）：
/// secret 默认掩码；reveal=true 解密并审计（会话已由鉴权中间件保证）。
async fn admin_config(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<ConfigQuery>,
) -> ApiResult<ConfigResp> {
    let (version, project, branch_name, structure_version, mut groups) = {
        let sm = app.sm.lock().expect("sm lock");
        let snap = sm
            .get_config(&ProjectId(pid.clone()), &BranchName(branch.clone()), 0)
            .map_err(ApiError::from)?;
        (
            snap.version,
            snap.project,
            snap.branch,
            snap.structure_version,
            snap.groups,
        )
    };
    apply_secret_policy(&mut groups, app.cipher.as_deref(), q.reveal);
    if q.reveal {
        app.audit
            .append(
                "config_reveal",
                Some(pid.clone()),
                Some(branch.clone()),
                Some(version),
                None,
                serde_json::json!({}),
            )
            .await;
    }
    Ok(Json(ConfigResp {
        project,
        branch: branch_name,
        version,
        structure_version,
        groups: plain_groups(&groups),
    }))
}

/// SDK 数据面快照（GET /v1/projects/{p}/branches/{b}/snapshot）：纯值输出；
/// secret 按数据面脱敏策略输出掩码（与 gRPC GetConfig masked 一致）。
async fn snapshot(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> ApiResult<ConfigResp> {
    let (version, project, branch_name, structure_version, mut groups) = {
        let sm = app.sm.lock().expect("sm lock");
        let snap = sm
            .get_config(&ProjectId(pid.clone()), &BranchName(branch.clone()), 0)
            .map_err(ApiError::from)?;
        (
            snap.version,
            snap.project,
            snap.branch,
            snap.structure_version,
            snap.groups,
        )
    };
    apply_secret_policy(&mut groups, app.cipher.as_deref(), false);
    Ok(Json(ConfigResp {
        project,
        branch: branch_name,
        version,
        structure_version,
        groups: plain_groups(&groups),
    }))
}

async fn version_history(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let versions = sm
        .version_history(&ProjectId(pid), &BranchName(branch))
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::to_value(versions).expect("serialize")))
}

// ---------------- 数据面快照（纯值输出） ----------------

/// Value → 纯 JSON（去掉 type 标签，供 SDK/应用消费）。
fn plain_value(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Json(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone())),
        Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
        Value::Secret(_) => serde_json::Value::String("***".into()),
    }
}

fn plain_groups(
    groups: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>>,
) -> serde_json::Value {
    let out: serde_json::Map<String, serde_json::Value> = groups
        .iter()
        .map(|(g, items)| {
            let m: serde_json::Map<String, serde_json::Value> = items
                .iter()
                .map(|(k, v)| (k.clone(), plain_value(v)))
                .collect();
            (g.clone(), serde_json::Value::Object(m))
        })
        .collect();
    serde_json::Value::Object(out)
}

// ---------------- 渲染（模块 08） ----------------

#[derive(Deserialize)]
struct RenderQuery {
    #[serde(default = "default_format")]
    format: String,
    /// 目标版本（0=活动版本）
    #[serde(default)]
    version: u64,
    /// 解密 secret 输出（需管理面会话 + 审计；默认掩码）
    #[serde(default)]
    reveal: bool,
}

fn default_format() -> String {
    "yaml".into()
}

/// 渲染配置文档（GET /v1/projects/{p}/branches/{b}/config?format=&version=&reveal=）。
/// secret 默认掩码（渲染器对密文输出 "***"）；reveal=true 需管理员会话（Bearer）+ 审计。
async fn render_config(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<RenderQuery>,
    req: axum::extract::Request,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorBody>)> {
    // reveal=true：校验管理面会话（本端点不在 /api/v1 鉴权中间件覆盖内，手动校验）
    let session_ok = {
        let auth = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.to_string());
        match auth {
            Some(t) => {
                let hash = dsh_core::token_hash(&t);
                let sm = app.sm.lock().expect("sm lock");
                match sm.get_session().ok().flatten() {
                    Some(s) => {
                        s.token_hash == hash && s.expires_at.map(|e| now_ms() < e).unwrap_or(true)
                    }
                    None => false,
                }
            }
            None => false,
        }
    };
    if q.reveal && !session_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorBody {
                code: "ERR_SESSION_EXPIRED".into(),
                message: "reveal=true 需要管理员会话".into(),
                detail: None,
            }),
        ));
    }
    let (version, mut groups) = {
        let sm = app.sm.lock().expect("sm lock");
        let snap = sm
            .get_config(
                &ProjectId(pid.clone()),
                &BranchName(branch.clone()),
                q.version,
            )
            .map_err(ApiError::from)?;
        (snap.version, snap.groups)
    };
    apply_secret_policy(&mut groups, app.cipher.as_deref(), q.reveal);
    if q.reveal {
        app.audit
            .append(
                "config_reveal",
                Some(pid.clone()),
                Some(branch.clone()),
                Some(version),
                None,
                serde_json::json!({ "format": q.format }),
            )
            .await;
    }
    let format = Format::parse(&q.format).map_err(ApiError)?;
    let body = Renderer.render(&groups, format).map_err(ApiError)?;
    let mime = match format {
        Format::Yaml => "application/yaml",
        Format::Toml => "application/toml",
        Format::Json => "application/json",
    };
    Ok(axum::response::Response::builder()
        .header("content-type", mime)
        .body(axum::body::Body::from(body))
        .expect("response"))
}

// ---------------- 会话（I7） ----------------

#[derive(Deserialize, Serialize)]
struct LoginReq {
    password: String,
}

#[derive(Serialize)]
struct LoginResp {
    token: String,
}

async fn login(
    State(app): State<ApiState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, (StatusCode, Json<ApiErrorBody>)> {
    // 密码校验：set-password 落状态机后优先；未设置时回退节点配置（--admin-password）。
    let sm_pw_ok = {
        let sm = app.sm.lock().expect("sm lock");
        match sm.get_admin_password_hash().ok().flatten() {
            Some(hash) => dsh_core::token_hash(&req.password) == hash,
            None => req.password == app.admin_password.as_ref(),
        }
    };
    if !sm_pw_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorBody {
                code: "ERR_FORBIDDEN".into(),
                message: "密码错误".into(),
                detail: None,
            }),
        ));
    }
    // 会话落 Raft 状态机（I7）：token 明文只在响应中返回一次，状态机仅存 SHA-256 哈希。
    // 非 leader 时跟随 leader_hint 转发到 leader 的公开 login 端点（跨节点唯一）。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let token = new_token();
        let hash = dsh_core::token_hash(&token);
        let ttl = app.session_ttl;
        let now = now_ms();
        let res = app
            .write(
                &Command::SessionLogin {
                    token_hash: hash,
                    issued_at: now,
                    expires_at: (ttl.as_secs() > 0).then(|| now + ttl.as_secs() as i64),
                },
                now,
            )
            .await;
        match res {
            Ok(_) => {
                app.audit
                    .append("login", None, None, None, None, serde_json::json!({}))
                    .await;
                return Ok(Json(LoginResp { token }));
            }
            Err(ApiError(e)) if e.kind == ErrorKind::LeaderRedirect => {
                let hint = e.leader_hint.unwrap_or_default();
                if !hint.is_empty() {
                    // NodeInfo.http_addr 无 scheme（如 127.0.0.1:8601）→ 转发前补 http://
                    let base = if hint.starts_with("http://") || hint.starts_with("https://") {
                        hint
                    } else {
                        format!("http://{hint}")
                    };
                    let client = reqwest::Client::new();
                    match client
                        .post(format!("{base}/api/v1/login"))
                        .json(&LoginReq {
                            password: req.password.clone(),
                        })
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            let status = resp.status();
                            let body: serde_json::Value =
                                resp.json().await.unwrap_or(serde_json::json!({}));
                            if status.is_success() {
                                let token = body
                                    .get("token")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                return Ok(Json(LoginResp { token }));
                            }
                            // 409 ERR_SESSION_IN_USE 等：原样转发 leader 的错误体
                            let code = body
                                .get("code")
                                .and_then(|c| c.as_str())
                                .unwrap_or("ERR_INTERNAL")
                                .to_string();
                            let message = body
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("login failed")
                                .to_string();
                            let detail = body.get("detail").cloned();
                            return Err((
                                status,
                                Json(ApiErrorBody {
                                    code,
                                    message,
                                    detail,
                                }),
                            ));
                        }
                        Err(_) => { /* leader 转发失败 → 重试 */ }
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                Json(ApiErrorBody {
                    code: "ERR_LEADER_REDIRECT".into(),
                    message: "login forwarding to leader timed out".into(),
                    detail: None,
                }),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn logout(
    State(app): State<ApiState>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    app.write(&Command::SessionLogout, now_ms()).await?;
    app.audit
        .append("logout", None, None, None, None, serde_json::json!({}))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn heartbeat(
    State(app): State<ApiState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiErrorBody>)> {
    let ttl = app.session_ttl;
    let now = now_ms();
    let expires = (ttl.as_secs() > 0).then(|| now + ttl.as_secs() as i64);
    app.write(
        &Command::SessionHeartbeat {
            expires_at: expires,
        },
        now,
    )
    .await?;
    Ok(Json(serde_json::json!({ "expires_at": expires })))
}

// ---------------- 可观测性（模块 10） ----------------

async fn metrics(State(app): State<ApiState>) -> String {
    metrics_text(&app.sm)
}

async fn readyz(State(app): State<ApiState>) -> Result<Json<serde_json::Value>, StatusCode> {
    if !is_ready(app.raft.as_ref()) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let state = app
        .raft
        .as_ref()
        .map(|r| format!("{:?}", r.metrics().borrow().state))
        .unwrap_or_else(|| "dev-single".into());
    Ok(Json(serde_json::json!({ "status": "ok", "state": state })))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

fn default_audit_limit() -> usize {
    100
}

/// 审计查询（读状态机落库）。
async fn audit_list(
    State(app): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let entries = sm
        .get_audit(q.action.as_deref(), q.since, q.limit)
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::to_value(entries).expect("serialize")))
}

// ---------------- 集群管理 ----------------

async fn cluster_members(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    if app.raft.is_none() {
        return Err(ApiError(dsh_core::Error::not_found("cluster mode")).into());
    }
    Ok(Json(cluster_members_json(app.raft.as_ref(), app.node_id)))
}

async fn cluster_join(
    State(app): State<ApiState>,
    Json(req): Json<JoinReq>,
) -> ApiResult<serde_json::Value> {
    let raft = app
        .raft
        .as_ref()
        .ok_or_else(|| ApiError(dsh_core::Error::not_found("cluster mode")))?;
    let node = RaftNodeInfo {
        grpc_addr: String::new(),
        http_addr: req.http_addr,
        raft_addr: req.raft_addr,
    };
    raft.add_learner(req.node_id, node, false)
        .await
        .map_err(|e| ApiError(dsh_core::Error::internal(e.to_string())))?;
    app.audit
        .append(
            "cluster_join",
            None,
            None,
            None,
            None,
            serde_json::json!({ "node_id": req.node_id }),
        )
        .await;
    Ok(Json(serde_json::json!({ "added_learner": req.node_id })))
}

async fn cluster_promote(
    State(app): State<ApiState>,
    Json(req): Json<JoinReq>,
) -> ApiResult<serde_json::Value> {
    let raft = app
        .raft
        .as_ref()
        .ok_or_else(|| ApiError(dsh_core::Error::not_found("cluster mode")))?;
    let metrics = raft.metrics().borrow().clone();
    let mut voters: Vec<u64> = metrics.membership_config.membership().voter_ids().collect();
    if !voters.contains(&req.node_id) {
        voters.push(req.node_id);
    }
    raft.change_membership(voters.clone(), false)
        .await
        .map_err(|e| ApiError(dsh_core::Error::internal(e.to_string())))?;
    app.audit
        .append(
            "cluster_promote",
            None,
            None,
            None,
            None,
            serde_json::json!({ "voters": voters }),
        )
        .await;
    Ok(Json(serde_json::json!({ "voters": voters })))
}

/// 移除节点（openapi /api/v1/cluster/remove）：voter 从投票集剔除（retain=false 一并移出成员表），
/// learner 用 RemoveNodes 直接移除；经 change_membership 提交。
async fn cluster_remove(
    State(app): State<ApiState>,
    Json(req): Json<JoinReq>,
) -> ApiResult<serde_json::Value> {
    use dsh_raft::openraft::ChangeMembers;
    use std::collections::BTreeSet;

    let raft = app
        .raft
        .as_ref()
        .ok_or_else(|| ApiError(dsh_core::Error::not_found("cluster mode")))?;
    let metrics = raft.metrics().borrow().clone();
    let m = metrics.membership_config.membership();
    let mut set = BTreeSet::new();
    set.insert(req.node_id);
    let voter_ids: Vec<u64> = m.voter_ids().collect();
    let node_ids: Vec<u64> = m.nodes().map(|(id, _)| *id).collect();
    let is_voter = voter_ids.contains(&req.node_id);
    let is_learner = node_ids.contains(&req.node_id);
    if !is_voter && !is_learner {
        return Err(ApiError(dsh_core::Error::validation("node not in membership")).into());
    }
    let changes = if is_voter {
        ChangeMembers::RemoveVoters(set)
    } else {
        ChangeMembers::RemoveNodes(set)
    };
    raft.change_membership(changes, false)
        .await
        .map_err(|e| ApiError(dsh_core::Error::internal(e.to_string())))?;
    let voters: Vec<u64> = raft
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .collect();
    app.audit
        .append(
            "cluster_remove",
            None,
            None,
            None,
            None,
            serde_json::json!({ "node_id": req.node_id, "voters": voters }),
        )
        .await;
    Ok(Json(serde_json::json!({ "voters": voters })))
}

// ---------------- 密钥管理（B6） ----------------

#[derive(Deserialize)]
struct RotateKeyReq {
    /// base64 32B 新主密钥
    new_key: String,
}

/// 轮换主密钥：新 KEK 成为当前（旧 KEK 保留，可解旧数据 CRY-002）；持久化环文件；触发审计。
async fn rotate_master_key(
    State(app): State<ApiState>,
    Json(req): Json<RotateKeyReq>,
) -> ApiResult<serde_json::Value> {
    let cipher = app
        .cipher
        .as_ref()
        .ok_or_else(|| ApiError(dsh_core::Error::validation("master key not configured")))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&req.new_key)
        .map_err(|e| ApiError(dsh_core::Error::validation(format!("new_key base64: {e}"))))?;
    if raw.len() != 32 {
        return Err(ApiError(dsh_core::Error::validation("new_key must be 32 bytes")).into());
    }
    let mut kek = [0u8; 32];
    kek.copy_from_slice(&raw);
    cipher.rotate_master_key(kek);
    // 持久化主密钥环（重启后可解旧数据）
    if let Some(path) = &app.ring_path {
        dsh_crypto::save_ring(path, &cipher.keyring())
            .map_err(|e| ApiError(dsh_core::Error::internal(e.to_string())))?;
    }
    let generation = cipher.keyring().generation();
    app.audit
        .append(
            "rotate_master_key",
            None,
            None,
            None,
            None,
            serde_json::json!({ "generation": generation }),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "ok": true, "generation": generation }),
    ))
}

// ---------------- 管理员运维（P2：force-logout / set-password / snapshot / retention-status） ----------------

/// 强制下线当前管理员会话（CLI `dsh admin force-logout` 兜底，design §9.3/I7）。
async fn admin_force_logout(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    app.write(&Command::SessionLogout, now_ms()).await?;
    app.audit
        .append(
            "force_logout",
            None,
            None,
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(Json(serde_json::json!({ "logged_out": true })))
}

#[derive(Deserialize)]
struct SetPasswordReq {
    password: String,
}

/// 修改管理员密码（哈希落状态机，集群一致；旧会话失效需重新登录）。
async fn admin_set_password(
    State(app): State<ApiState>,
    Json(req): Json<SetPasswordReq>,
) -> ApiResult<serde_json::Value> {
    if req.password.len() < 6 {
        return Err(ApiError(dsh_core::Error::validation("密码至少 6 位")).into());
    }
    let hash = dsh_core::token_hash(&req.password);
    app.write(
        &Command::AdminSetPassword {
            password_hash: hash,
        },
        now_ms(),
    )
    .await?;
    // 改密后强制下线当前会话（旧 token 失效）
    app.write(&Command::SessionLogout, now_ms()).await?;
    app.audit
        .append(
            "set_password",
            None,
            None,
            None,
            None,
            serde_json::json!({}),
        )
        .await;
    Ok(Json(serde_json::json!({ "changed": true })))
}

/// 触发备份快照：返回状态机全量 KV dump（`dsh admin snapshot` 备份用；恢复走 dump/restore）。
async fn admin_snapshot(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    let pairs = {
        let sm = app.sm.lock().expect("sm lock");
        sm.dump_all().map_err(ApiError::from)?
    };
    let entries: Vec<serde_json::Value> = pairs
        .iter()
        .map(|(k, v)| {
            serde_json::json!({
                "key": String::from_utf8_lossy(k),
                "value": String::from_utf8_lossy(v),
            })
        })
        .collect();
    app.audit
        .append(
            "snapshot",
            None,
            None,
            None,
            None,
            serde_json::json!({ "entries": entries.len() }),
        )
        .await;
    Ok(Json(serde_json::json!({
        "version": 1,
        "kind": "dsh-state-dump",
        "entries": entries,
    })))
}

/// 保留策略状态（`dsh admin version-retention-status`）：配置值 + 当前版本/审计计数。
async fn admin_retention_status(State(app): State<ApiState>) -> ApiResult<serde_json::Value> {
    let (projects, versions, audits) = {
        let sm = app.sm.lock().expect("sm lock");
        let projects = sm.list_projects().map(|p| p.len()).unwrap_or(0);
        let mut versions = 0u64;
        if let Ok(plist) = sm.list_projects() {
            for p in plist {
                if let Ok(bs) = sm.list_branches(&p.id) {
                    for b in bs {
                        if let Ok(Some(st)) = sm.get_branch_state(&p.id, &b) {
                            versions += st.active_version;
                        }
                    }
                }
            }
        }
        let audits = sm
            .get_audit(None, None, 1)
            .map(|v| v.first().map(|e| e.seq).unwrap_or(0))
            .unwrap_or(0);
        (projects, versions, audits)
    };
    Ok(Json(serde_json::json!({
        "version_retention": app.version_retention,
        "audit_retention": app.audit_retention,
        "projects": projects,
        "active_versions": versions,
        "audit_entries": audits,
        "hint": "version_retention=0 表示全量保留；audit_retention=0 表示不裁剪",
    })))
}

// ---------------- watch（模块 06） ----------------

#[derive(Deserialize)]
struct WatchQuery {
    /// 断线续传起点：重放该版本之后的历史事件再转实时（0=仅实时）
    #[serde(default)]
    after_version: u64,
}

async fn watch_branch(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<WatchQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // after_version > 0：按版本链合成历史事件（相邻快照 diff；与 gRPC Watch 重放一致）
    let replay = {
        let mut out: Vec<dsh_core::model::PublishEvent> = Vec::new();
        if q.after_version > 0 {
            let sm = app.sm.lock().expect("sm lock");
            let pid = ProjectId(pid.clone());
            let bname = BranchName(branch.clone());
            if let Ok(hist) = sm.version_history(&pid, &bname) {
                let mut prev: dsh_core::model::SnapshotMap = Default::default();
                for rec in hist {
                    if rec.no <= q.after_version {
                        continue;
                    }
                    if let Ok(cur) = sm.snapshot_of(&pid, &bname, rec.no) {
                        let diff = dsh_core::diff::compute_diff(&prev, &cur);
                        prev = cur;
                        out.push(dsh_core::model::PublishEvent {
                            project: pid.clone(),
                            branch: bname.clone(),
                            version: rec.no,
                            ty: if rec.rollback_of.is_some() {
                                dsh_core::model::EventType::Rollback
                            } else {
                                dsh_core::model::EventType::ValuePublish
                            },
                            structure_version: rec.structure_version,
                            comment: rec.comment,
                            request_id: String::new(),
                            changes: diff,
                        });
                    }
                }
            }
        }
        out
    };
    watch_sse(app.hub.subscribe(), &pid, &branch, replay)
}

// ---------------- 工具 ----------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "auto-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// 会话令牌（128-bit 随机 hex；rand 提供 CSPRNG 熵）。
fn new_token() -> String {
    let b: [u8; 16] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------- 路由 ----------------

pub fn build_router(app: ApiState) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/admin", get(admin_index))
        .route("/admin/{*path}", get(admin_static))
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/heartbeat", post(heartbeat))
        .route("/api/v1/audit", get(audit_list))
        .route("/api/v1/admin/rotate-master-key", post(rotate_master_key))
        .route("/api/v1/admin/force-logout", post(admin_force_logout))
        .route("/api/v1/admin/set-password", post(admin_set_password))
        .route("/api/v1/admin/snapshot", get(admin_snapshot))
        .route(
            "/api/v1/admin/retention-status",
            get(admin_retention_status),
        )
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{p}",
            get(project_detail).delete(delete_project),
        )
        .route(
            "/api/v1/projects/{p}/branches",
            get(list_branches).post(create_branch),
        )
        .route(
            "/api/v1/projects/{p}/branches/{b}",
            get(branch_detail).delete(delete_branch),
        )
        .route("/api/v1/projects/{p}/diff", get(branch_diff))
        .route("/api/v1/projects/{p}/promote", post(promote))
        .route("/api/v1/shared", get(list_shared).post(create_shared))
        .route(
            "/api/v1/shared-draft",
            get(list_shared_drafts).put(update_shared_draft),
        )
        .route("/api/v1/shared/publish", post(publish_shared))
        .route(
            "/api/v1/shared/refs",
            get(list_refs).post(ref_bind).delete(ref_unbind),
        )
        .route(
            "/api/v1/projects/{p}/structure-draft",
            get(get_structure_draft).put(set_structure_draft),
        )
        .route(
            "/api/v1/projects/{p}/structure-draft/publish",
            post(publish_structure),
        )
        .route("/api/v1/projects/{p}/branches/{b}/draft", put(update_draft))
        .route("/api/v1/projects/{p}/branches/{b}/publish", post(publish))
        .route("/api/v1/projects/{p}/branches/{b}/rollback", post(rollback))
        .route(
            "/api/v1/projects/{p}/branches/{b}/config",
            get(admin_config),
        )
        .route(
            "/api/v1/projects/{p}/branches/{b}/versions",
            get(version_history),
        )
        // 数据面快照（SDK get 用；无鉴权，面向应用拉取；secret 脱敏输出）
        .route("/v1/projects/{p}/branches/{b}/snapshot", get(snapshot))
        .route("/v1/projects/{p}/branches/{b}/config", get(render_config))
        .route("/v1/projects/{p}/branches/{b}/watch", get(watch_branch));
    if app.raft.is_some() {
        router = router
            .route("/api/v1/cluster/members", get(cluster_members))
            .route("/api/v1/cluster/join", post(cluster_join))
            .route("/api/v1/cluster/promote", post(cluster_promote))
            .route("/api/v1/cluster/remove", post(cluster_remove));
    }
    router = router.layer(axum::middleware::from_fn_with_state(
        app.clone(),
        auth_middleware,
    ));
    router = router.layer(axum::middleware::from_fn(security_headers));
    router.with_state(app)
}
