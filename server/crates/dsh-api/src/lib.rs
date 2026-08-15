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
use axum::routing::{delete, get, post, put};
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

/// 会话主体解析结果（中间件与 render_config 共用，N15：禁止第三份解析实现）。
fn resolve_principal(
    app: &ApiState,
    auth_header: Option<&str>,
) -> Result<dsh_core::Principal, ()> {
    let token = auth_header
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(())?;
    let hash = dsh_core::token_hash(token);
    let sm = app.sm.lock().expect("sm lock");
    // token 前缀路由（§3）：pa.{username}.{secret} → sess/pa/{username}；
    // adm.{secret} 或无前缀（旧格式 fallback）→ sess/admin。
    let (principal, session) = if let Some(rest) = token.strip_prefix("pa.") {
        let username = rest.split('.').next().unwrap_or("");
        let session = sm.get_pa_session(username).ok().flatten();
        (
            dsh_core::Principal::ProjectAdmin {
                username: username.to_string(),
                project: dsh_core::ProjectId(String::new()),
            },
            session,
        )
    } else {
        (
            dsh_core::Principal::Admin,
            sm.get_session().ok().flatten(),
        )
    };
    match session {
        Some(s) => {
            let hash_ok = s.token_hash == hash;
            let not_expired = s.expires_at.map(|e| now_ms() < e).unwrap_or(true);
            if !(hash_ok && not_expired) {
                return Err(());
            }
            // 以状态机中存储的 principal 为准（PA 的 project 归属不可由 token 伪造）
            Ok(s.principal)
        }
        None => Err(()),
    }
}

/// 从未解码 path 提取 /api/v1/projects/{p}/... 的 {p} 段（N2：URL 编码/大写/特殊字符
/// 不通过 valid_name → None → 对 PA 默认拒绝）。
fn project_segment(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/v1/projects/")?;
    let seg = rest.split('/').next()?;
    if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        Some(seg.to_string())
    } else {
        None
    }
}

/// 项目管理员授权矩阵（§4：默认拒绝、显式放行）。
/// 返回 true=放行。全局管理员永远放行（调用方判断）。
fn pa_allowed(principal: &dsh_core::Principal, method: &str, path: &str) -> bool {
    let dsh_core::Principal::ProjectAdmin { project, .. } = principal else {
        return true; // 全局管理员
    };
    let pid = &project.0;

    // 自身会话（显式放行，防锁死 B5）
    if (method == "POST" && path == "/api/v1/logout")
        || (method == "POST" && path == "/api/v1/heartbeat")
    {
        return true;
    }
    // 读项目列表（handler 内过滤为自己项目）
    if method == "GET" && path == "/api/v1/projects" {
        return true;
    }
    // 审计（handler 内强制过滤 project）
    if method == "GET" && path == "/api/v1/audit" {
        return true;
    }
    // 共享引用只读（handler 内强制覆写 project，N11）
    if method == "GET" && path == "/api/v1/shared/refs" {
        return true;
    }
    // 项目本地端点：/api/v1/projects/{p}/... 且 p == 自己项目
    if let Some(p) = project_segment(path) {
        if &p != pid {
            return false; // 跨项目
        }
        // PA 账号管理端点（全局面）拒绝
        if path
            .strip_prefix(&format!("/api/v1/projects/{pid}"))
            .is_some_and(|r| r.starts_with("/admins"))
        {
            return false;
        }
        // 项目面操作拒绝：删除/创建项目本身（即使 {p}==自己项目也不许自毁/建新项目）
        if method == "DELETE" && path == format!("/api/v1/projects/{pid}") {
            return false;
        }
        // 其余项目本地端点全部放行（结构/值/分支/版本/对比/promote/diff/config/reveal）
        return true;
    }
    // 非项目路径：默认拒绝（共享写、集群、全局 admin、账号等）
    false
}

/// 管理面鉴权中间件（全局管理员 + 项目管理员双主体；除 login/healthz/readyz 外均需 Bearer）。
async fn auth_middleware(
    State(app): State<ApiState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorBody>)> {
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_string();
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
            .map(|t| t.to_string());
        let principal = match resolve_principal(&app, auth.as_deref()) {
            Ok(p) => p,
            Err(_) => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ApiErrorBody {
                        code: "ERR_SESSION_EXPIRED".into(),
                        message: "需要管理员会话".into(),
                        detail: None,
                    }),
                ));
            }
        };
        // 授权矩阵：项目管理员默认拒绝、显式放行（§4）
        if !pa_allowed(&principal, &method, &path) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorBody {
                    code: "ERR_FORBIDDEN".into(),
                    message: "项目管理员无权访问该资源".into(),
                    detail: None,
                }),
            ));
        }
        req.extensions_mut().insert(principal);
    }
    Ok(next.run(req).await)
}

async fn create_project(
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    Json(req): Json<CreateProjectReq>,
) -> ApiResult<serde_json::Value> {
    let pid = ProjectId(req.name.clone());
    app.write(&Command::ProjectCreate {
                name: req.name.clone(),
                operator: "admin".to_string(),
            }, now_ms())
        .await?;
    app.audit
        .append(
            "project_create",
            Some(pid.as_str().into()),
            None,
            None,
            None,
            serde_json::json!({}),
            &principal_op(&principal),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "id": pid.as_str(), "branches": ["dev", "test", "prod"] }),
    ))
}

async fn list_projects(
    State(app): State<ApiState>,
    principal: axum::Extension<dsh_core::Principal>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let mut projects = sm
        .list_projects()
        .map_err(ApiError::from)?
        .into_iter()
        .map(|p| serde_json::json!({ "id": p.id.as_str(), "name": p.name }))
        .collect::<Vec<_>>();
    // PA 只能看到自己项目（§4：handler 层过滤）
    if let dsh_core::Principal::ProjectAdmin { project, .. } = principal.0 {
        projects.retain(|p| p.get("id").and_then(|i| i.as_str()) == Some(project.0.as_str()));
    }
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
    principal: axum::Extension<dsh_core::Principal>,
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
        
            operator: principal_op(&principal),
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<StructureDraftReq>,
) -> ApiResult<serde_json::Value> {
    app.write(
        &Command::StructureDraftSet {
            project: ProjectId(pid.clone()),
            base_version: req.base_version,
            groups: req.groups,
        
            operator: principal_op(&principal),
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
            &principal_op(&principal),
        )
        .await;
    Ok(Json(serde_json::json!({ "saved": true, "project": pid })))
}

async fn publish_structure(
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<PublishReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let outcome = app
        .publish
        .publish_structure(&ProjectId(pid.clone()), &req.comment, &rid, &principal_op(&principal))
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
            &principal_op(&principal),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "affected_branches": affected, "request_id": rid }),
    ))
}

async fn update_draft(
    principal: axum::Extension<dsh_core::Principal>,
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
        .update_draft(&pid_obj, &branch_obj, req.updates, deletes, &principal_op(&principal))
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
            &principal_op(&principal),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "saved": true, "project": pid, "branch": branch }),
    ))
}

async fn publish(
    principal: axum::Extension<dsh_core::Principal>,
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
            &principal_op(&principal),
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
            &principal_op(&principal),
        )
        .await;
    Ok(Json(serde_json::json!({
        "version": outcome.version,
        "changes": serde_json::to_value(&outcome.changes).expect("serialize"),
        "request_id": rid,
    })))
}

async fn rollback(
    principal: axum::Extension<dsh_core::Principal>,
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
            &principal_op(&principal),
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
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
    app.write(&Command::ProjectDelete {
                id: pid_obj,
                operator: "admin".to_string(),
            }, now_ms())
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    app.write(
        &Command::BranchDelete {
            project: ProjectId(pid.clone()),
            name: BranchName(branch.clone()),
        
            operator: principal_op(&principal),
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
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
            .update_draft(&pid_obj, &to_b, updates, vec![], &principal_op(&principal))
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
            &principal_op(&principal),
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
    app.write(&Command::SharedDraftUpdate {
                item,
                operator: "admin".to_string(),
            }, now_ms())
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
            "admin",
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
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    Json(req): Json<PublishReq>,
) -> ApiResult<serde_json::Value> {
    let rid = req.request_id.unwrap_or_else(new_request_id);
    let outcome = app
        .write(
            &Command::SharedPublish {
                comment: req.comment,
                request_id: rid.clone(),
            
                operator: principal_op(&principal),
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
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
        
            operator: principal_op(&principal),
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
            &principal_op(&principal),
        )
        .await;
    Ok(Json(
        serde_json::json!({ "bound": true, "project": req.project }),
    ))
}

async fn ref_unbind(
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    Json(req): Json<RefUnbindReq>,
) -> ApiResult<serde_json::Value> {
    app.write(
        &Command::RefUnbind {
            project: ProjectId(req.project.clone()),
            group: req.group.clone(),
            item_key: req.item_key.clone(),
        
            operator: principal_op(&principal),
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
            &principal_op(&principal),
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
    principal: axum::Extension<dsh_core::Principal>,
    axum::extract::Query(mut q): axum::extract::Query<RefsQuery>,
) -> ApiResult<serde_json::Value> {
    // N11：PA 强制覆写 project 为自己项目（防跨项目绑定元数据读取）
    if let dsh_core::Principal::ProjectAdmin { project, .. } = principal.0 {
        q.project = project.0;
    }
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
    principal: axum::Extension<dsh_core::Principal>,
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
                &principal_op(&principal),
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

// ---------------- 项目管理员账号管理（§5，全局管理员专用；中间件已对 PA 拒绝 /admins 路径）----------------

#[derive(Deserialize)]
struct ProjectAdminCreateReq {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct ProjectAdminResp {
    username: String,
    project: String,
    created_at: i64,
}

/// 生成盐 + 加盐哈希（sha256(salt || password)，§2）。
fn salted_password_hash(password: &str) -> (String, String) {
    use rand::RngCore;
    let mut salt_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt_bytes);
    let salt = salt_bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let hash = dsh_core::token_hash(&format!("{salt}{password}"));
    (salt, hash)
}

async fn create_project_admin(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<ProjectAdminCreateReq>,
) -> Result<(StatusCode, Json<ProjectAdminResp>), (StatusCode, Json<ApiErrorBody>)> {
    let now = now_ms();
    let (salt, hash) = salted_password_hash(&req.password);
    let cmd = Command::ProjectAdminCreate {
        project: ProjectId(pid.clone()),
        username: req.username.clone(),
        salt,
        password_hash: hash,
    };
    match app.write(&cmd, now).await {
        Ok(_) => {
            app.audit
                .append(
                    "project_admin_create",
                    Some(pid.clone()),
                    None,
                    None,
                    None,
                    serde_json::json!({"username": req.username}),
                    "admin",
                )
                .await;
            // 响应用请求值构造（集群模式下 write 返回与本地 apply 读回存在竞态，禁 expect）
            Ok((
                StatusCode::CREATED,
                Json(ProjectAdminResp {
                    username: req.username,
                    project: pid.clone(),
                    created_at: now,
                }),
            ))
        }
        Err(e) if e.0.kind == ErrorKind::Conflict => Err((
            StatusCode::CONFLICT,
            Json(ApiErrorBody {
                code: "ERR_ACCOUNT_EXISTS".into(),
                message: e.0.message.clone(),
                detail: e.0.detail.clone(),
            }),
        )),
        Err(e) => Err(e.into()),
    }
}

async fn list_project_admins(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    let accounts = sm
        .list_project_admins(&pid)
        .map_err(ApiError::from)?
        .into_iter()
        .map(|a| serde_json::json!({ "username": a.username, "created_at": a.created_at }))
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!(accounts)))
}

async fn delete_project_admin(
    State(app): State<ApiState>,
    AxumPath((pid, username)): AxumPath<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    match app
        .write(
            &Command::ProjectAdminDelete {
                username: username.clone(),
            },
            now_ms(),
        )
        .await
    {
        Ok(_) => {
            app.audit
                .append(
                    "project_admin_delete",
                    Some(pid),
                    None,
                    None,
                    None,
                    serde_json::json!({"username": username}),
                    "admin",
                )
                .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) if e.0.kind == ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorBody {
                code: "ERR_ACCOUNT_NOT_FOUND".into(),
                message: e.0.message.clone(),
                detail: e.0.detail.clone(),
            }),
        )),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
struct ProjectAdminSetPasswordReq {
    password: String,
}

async fn set_project_admin_password(
    State(app): State<ApiState>,
    AxumPath((pid, username)): AxumPath<(String, String)>,
    Json(req): Json<ProjectAdminSetPasswordReq>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    let (salt, hash) = salted_password_hash(&req.password);
    match app
        .write(
            &Command::ProjectAdminSetPassword {
                username: username.clone(),
                salt,
                password_hash: hash,
            },
            now_ms(),
        )
        .await
    {
        Ok(_) => {
            app.audit
                .append(
                    "project_admin_set_password",
                    Some(pid),
                    None,
                    None,
                    None,
                    serde_json::json!({"username": username}),
                    "admin",
                )
                .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) if e.0.kind == ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorBody {
                code: "ERR_ACCOUNT_NOT_FOUND".into(),
                message: e.0.message.clone(),
                detail: e.0.detail.clone(),
            }),
        )),
        Err(e) => Err(e.into()),
    }
}
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
    // reveal=true：校验管理面会话（本端点不在 /api/v1 鉴权中间件覆盖内，手动校验）。
    // B2 修复：走共享 resolve_principal（与中间件同一实现，N15），PA 只能 reveal 自己项目。
    // 区分两种失败：无有效会话 → 401；已认证但越权 → 403。
    let principal = resolve_principal(
        &app,
        req.headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );
    let session_ok = match &principal {
        Ok(dsh_core::Principal::Admin) => true,
        Ok(dsh_core::Principal::ProjectAdmin { project, .. }) => project.0 == pid,
        Err(_) => false,
    };
    if q.reveal {
        if principal.is_err() {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ApiErrorBody {
                    code: "ERR_SESSION_EXPIRED".into(),
                    message: "reveal=true 需要管理员会话".into(),
                    detail: None,
                }),
            ));
        }
        if !session_ok {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorBody {
                    code: "ERR_FORBIDDEN".into(),
                    message: "项目管理员只能查看本项目的配置明文".into(),
                    detail: None,
                }),
            ));
        }
    }
    let _ = session_ok;
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
                &principal
                    .as_ref()
                    .map(|p| principal_op(p))
                    .unwrap_or_else(|_| "admin".to_string()),
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
    /// 项目管理员用户名（缺省/空 = 全局管理员，向后兼容）。
    #[serde(default)]
    username: Option<String>,
}

#[derive(Serialize)]
struct LoginResp {
    token: String,
    /// "admin" | "project_admin"（调用方感知身份）。
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

/// 生成项目管理员 token：pa.{username}.{secret}（§3 token 前缀路由）。
fn new_pa_token(username: &str) -> String {
    format!("pa.{username}.{}", new_token())
}

async fn login(
    State(app): State<ApiState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, (StatusCode, Json<ApiErrorBody>)> {
    // 项目管理员登录分支（§3）：username 非空 → 校验 adm/pa/{username}。
    let pa_username = req
        .username
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    if let Some(username) = pa_username {
        return pa_login(app, req, username).await;
    }
    // 密码校验：set-password 落状态机后优先；未设置时回退节点配置（--admin-password）。
    let sm_pw_ok = {
        let sm = app.sm.lock().expect("sm lock");
        match sm.get_admin_password_hash().ok().flatten() {
            Some(hash) => dsh_core::token_hash(&req.password) == hash,
            None => req.password == app.admin_password.as_ref(),
        }
    };
    if !sm_pw_ok {
        app.audit
            .append(
                "login_failed",
                None,
                None,
                None,
                None,
                serde_json::json!({}),
                "admin",
            )
            .await;
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorBody {
                // N4：登录失败统一码（与 403 ERR_FORBIDDEN 区分），文案不区分账号是否存在（防枚举）
                code: "ERR_BAD_CREDENTIALS".into(),
                message: "账号或密码错误".into(),
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
                    .append("login", None, None, None, None, serde_json::json!({}), "admin")
                    .await;
                return Ok(Json(LoginResp {
                    token,
                    role: Some("admin".into()),
                    project: None,
                }));
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
                    // N1：转发体透传 username（PA 登录不能在非 leader 节点被当 admin 路径）
                    let fwd = LoginReq {
                        password: req.password.clone(),
                        username: req.username.clone(),
                    };
                    match client
                        .post(format!("{base}/api/v1/login"))
                        .json(&fwd)
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
                                let role = body
                                    .get("role")
                                    .and_then(|r| r.as_str())
                                    .map(str::to_string);
                                let project = body
                                    .get("project")
                                    .and_then(|p| p.as_str())
                                    .map(str::to_string);
                                return Ok(Json(LoginResp { token, role, project }));
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

/// 项目管理员登录（§3）：加盐哈希校验 → PaSessionLogin（apply 判 SessionInUse，不读时钟）；
/// SessionInUse 时复查会话已过期则先登出重试一轮（N13，过期重登在 API 层组合）。
async fn pa_login(
    app: ApiState,
    req: LoginReq,
    username: String,
) -> Result<Json<LoginResp>, (StatusCode, Json<ApiErrorBody>)> {
    // 校验账号与密码（统一 401，防枚举）
    let account = {
        let sm = app.sm.lock().expect("sm lock");
        sm.get_project_admin(&username).ok().flatten()
    };
    let ok = account
        .as_ref()
        .map(|acct| {
            let salted = format!("{}{}", acct.salt, req.password);
            dsh_core::token_hash(&salted) == acct.password_hash
        })
        .unwrap_or(false);
    if !ok {
        app.audit
            .append(
                "login_failed",
                None,
                None,
                None,
                None,
                serde_json::json!({"username": username}),
                &format!("pa:{username}"),
            )
            .await;
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorBody {
                code: "ERR_BAD_CREDENTIALS".into(),
                message: "账号或密码错误".into(),
                detail: None,
            }),
        ));
    }
    let project = account.map(|a| a.project.0).unwrap_or_default();
    let operator = format!("pa:{username}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut retried_expired = false;
    loop {
        let token = new_pa_token(&username);
        let hash = dsh_core::token_hash(&token);
        let ttl = app.session_ttl;
        let now = now_ms();
        let res = app
            .write(
                &Command::PaSessionLogin {
                    username: username.clone(),
                    token_hash: hash,
                    issued_at: now,
                    expires_at: (ttl.as_secs() > 0).then(|| now + ttl.as_secs() as i64),
                    device_id: "cli".into(),
                },
                now,
            )
            .await;
        match res {
            Ok(_) => {
                app.audit
                    .append(
                        "login",
                        Some(project.clone()),
                        None,
                        None,
                        None,
                        serde_json::json!({"username": username}),
                        &operator,
                    )
                    .await;
                return Ok(Json(LoginResp {
                    token,
                    role: Some("project_admin".into()),
                    project: Some(project),
                }));
            }
            Err(ApiError(e)) if e.kind == ErrorKind::SessionInUse => {
                // N13：已有会话已过期 → 先登出再重试一轮（仅一次）
                let expired = {
                    let sm = app.sm.lock().expect("sm lock");
                    sm.get_pa_session(&username)
                        .ok()
                        .flatten()
                        .and_then(|s| s.expires_at)
                        .map(|exp| now_ms() >= exp)
                        .unwrap_or(false)
                };
                if expired && !retried_expired {
                    retried_expired = true;
                    let _ = app
                        .write(
                            &Command::PaSessionLogout {
                                username: username.clone(),
                            },
                            now_ms(),
                        )
                        .await;
                    continue;
                }
                return Err((
                    StatusCode::CONFLICT,
                    Json(ApiErrorBody {
                        code: "ERR_SESSION_IN_USE".into(),
                        message: "该账号已有会话在线".into(),
                        detail: None,
                    }),
                ));
            }
            Err(ApiError(e)) if e.kind == ErrorKind::LeaderRedirect => {
                // 非 leader：转发到 leader（透传 username）
                let hint = e.leader_hint.unwrap_or_default();
                if !hint.is_empty() {
                    let base = if hint.starts_with("http://") || hint.starts_with("https://") {
                        hint
                    } else {
                        format!("http://{hint}")
                    };
                    let client = reqwest::Client::new();
                    let fwd = LoginReq {
                        password: req.password.clone(),
                        username: Some(username.clone()),
                    };
                    if let Ok(resp) = client
                        .post(format!("{base}/api/v1/login"))
                        .json(&fwd)
                        .send()
                        .await
                    {
                        let status = resp.status();
                        let body: serde_json::Value =
                            resp.json().await.unwrap_or(serde_json::json!({}));
                        if status.is_success() {
                            return Ok(Json(LoginResp {
                                token: body
                                    .get("token")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                role: body.get("role").and_then(|r| r.as_str()).map(str::to_string),
                                project: body
                                    .get("project")
                                    .and_then(|p| p.as_str())
                                    .map(str::to_string),
                            }));
                        }
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
                        return Err((status, Json(ApiErrorBody { code, message, detail })));
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
    principal: axum::Extension<dsh_core::Principal>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    let operator = principal_op(&principal);
    match principal.0 {
        dsh_core::Principal::Admin => {
            app.write(&Command::SessionLogout, now_ms()).await?;
        }
        dsh_core::Principal::ProjectAdmin { ref username, .. } => {
            app.write(
                &Command::PaSessionLogout {
                    username: username.clone(),
                },
                now_ms(),
            )
            .await?;
        }
    }
    app.audit
        .append("logout", None, None, None, None, serde_json::json!({}), &operator)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Principal → 审计 operator 值（"admin" / "pa:{username}"）。
fn principal_op(p: &dsh_core::Principal) -> String {
    match p {
        dsh_core::Principal::Admin => "admin".into(),
        dsh_core::Principal::ProjectAdmin { username, .. } => format!("pa:{username}"),
    }
}

async fn heartbeat(
    State(app): State<ApiState>,
    principal: axum::Extension<dsh_core::Principal>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiErrorBody>)> {
    let ttl = app.session_ttl;
    let now = now_ms();
    let expires = (ttl.as_secs() > 0).then(|| now + ttl.as_secs() as i64);
    match principal.0 {
        dsh_core::Principal::Admin => {
            app.write(
                &Command::SessionHeartbeat {
                    expires_at: expires,
                },
                now,
            )
            .await?;
        }
        dsh_core::Principal::ProjectAdmin { ref username, .. } => {
            app.write(
                &Command::PaSessionHeartbeat {
                    username: username.clone(),
                    expires_at: expires,
                },
                now,
            )
            .await?;
        }
    }
    Ok(Json(serde_json::json!({ "expires_at": expires })))
}

// ---------------- 可观测性（模块 10） ----------------

async fn metrics(State(app): State<ApiState>) -> String {
    // 聚合判定：全局管理员或任一项目管理员会话活动即 1（N7）
    let session_active = {
        let sm = app.sm.lock().expect("sm lock");
        let admin_active = sm
            .get_session()
            .ok()
            .flatten()
            .map(|s| s.expires_at.map(|e| now_ms() < e).unwrap_or(true))
            .unwrap_or(false);
        admin_active || sm.any_pa_session_active(now_ms())
    };
    metrics_text(
        &app.sm,
        app.raft.as_ref(),
        session_active,
        app.cipher.is_some(),
    )
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
    principal: axum::Extension<dsh_core::Principal>,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> ApiResult<serde_json::Value> {
    let sm = app.sm.lock().expect("sm lock");
    // PA 强制下推 project 过滤到状态机（§4 + R2：先截断后过滤会让 PA 视图被全局条目冲空）
    let project_filter = match principal.0 {
        dsh_core::Principal::Admin => None,
        dsh_core::Principal::ProjectAdmin { project, .. } => Some(project.0),
    };
    let entries = sm
        .get_audit(q.action.as_deref(), project_filter.as_deref(), q.since, q.limit)
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
            "admin",
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
            "admin",
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
            "admin",
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
            "admin",
        )
        .await;
    Ok(Json(
        serde_json::json!({ "ok": true, "generation": generation }),
    ))
}

// ---------------- 管理员运维（P2：force-logout / set-password / snapshot / retention-status） ----------------

#[derive(Deserialize, Default)]
struct ForceLogoutReq {
    /// 缺省 = 踢全局管理员会话；指定 username = 踢对应项目管理员会话（N16）。
    #[serde(default)]
    username: Option<String>,
}

/// 强制下线会话（CLI `dsh admin force-logout` 兜底，design §9.3/I7）。
async fn admin_force_logout(
    State(app): State<ApiState>,
    axum::extract::Json(req): axum::extract::Json<ForceLogoutReq>,
) -> ApiResult<serde_json::Value> {
    match req.username.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => {
            app.write(&Command::SessionLogout, now_ms()).await?;
        }
        Some(username) => {
            // 踢 PA 会话（账号本体不动；不存在账号也幂等成功——会话本就无）
            app.write(
                &Command::PaSessionLogout {
                    username: username.to_string(),
                },
                now_ms(),
            )
            .await?;
        }
    }
    app.audit
        .append(
            "force_logout",
            None,
            None,
            None,
            None,
            serde_json::json!({ "username": req.username }),
            "admin",
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
            "admin",
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
            "admin",
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
            .get_audit(None, None, None, 1)
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
            "/api/v1/projects/{p}/admins",
            get(list_project_admins).post(create_project_admin),
        )
        .route(
            "/api/v1/projects/{p}/admins/{u}",
            delete(delete_project_admin).put(set_project_admin_password),
        )
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
