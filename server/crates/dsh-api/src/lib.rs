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
use dsh_core::model::{BranchName, GroupDef, ProjectId, Value};
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

async fn get_config(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> ApiResult<ConfigResp> {
    let sm = app.sm.lock().expect("sm lock");
    let snap = sm
        .get_config(&ProjectId(pid.clone()), &BranchName(branch.clone()), 0)
        .map_err(ApiError::from)?;
    // 解密 secret（输出明文；需主密钥）
    let mut groups = snap.groups;
    if let Some(cipher) = &app.cipher {
        for items in groups.values_mut() {
            for v in items.values_mut() {
                if let Value::Secret(ct) = v {
                    let plain = cipher.decrypt_secret(ct);
                    *v = match plain {
                        Ok(p) => Value::String(String::from_utf8_lossy(&p).into_owned()),
                        Err(_) => Value::String("***".into()),
                    };
                }
            }
        }
    }
    Ok(Json(ConfigResp {
        project: snap.project,
        branch: snap.branch,
        version: snap.version,
        structure_version: snap.structure_version,
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
}

fn default_format() -> String {
    "yaml".into()
}

async fn render_config(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<RenderQuery>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorBody>)> {
    let sm = app.sm.lock().expect("sm lock");
    let snap = sm
        .get_config(&ProjectId(pid.clone()), &BranchName(branch.clone()), 0)
        .map_err(ApiError::from)?;
    let mut groups = snap.groups;
    if let Some(cipher) = &app.cipher {
        for items in groups.values_mut() {
            for v in items.values_mut() {
                if let Value::Secret(ct) = v {
                    let plain = cipher.decrypt_secret(ct);
                    *v = match plain {
                        Ok(p) => Value::String(String::from_utf8_lossy(&p).into_owned()),
                        Err(_) => Value::String("***".into()),
                    };
                }
            }
        }
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
    if req.password != app.admin_password.as_ref() {
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

// ---------------- watch（模块 06） ----------------

async fn watch_branch(
    State(app): State<ApiState>,
    AxumPath((pid, branch)): AxumPath<(String, String)>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    watch_sse(app.hub.subscribe(), &pid, &branch)
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
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{p}/branches",
            get(list_branches).post(create_branch),
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
        .route("/api/v1/projects/{p}/branches/{b}/config", get(get_config))
        .route(
            "/api/v1/projects/{p}/branches/{b}/versions",
            get(version_history),
        )
        // 数据面快照（SDK get 用；无鉴权，面向应用拉取）
        .route("/v1/projects/{p}/branches/{b}/snapshot", get(get_config))
        .route("/v1/projects/{p}/branches/{b}/config", get(render_config))
        .route("/v1/projects/{p}/branches/{b}/watch", get(watch_branch));
    if app.raft.is_some() {
        router = router
            .route("/api/v1/cluster/members", get(cluster_members))
            .route("/api/v1/cluster/join", post(cluster_join))
            .route("/api/v1/cluster/promote", post(cluster_promote));
    }
    router = router.layer(axum::middleware::from_fn_with_state(
        app.clone(),
        auth_middleware,
    ));
    router = router.layer(axum::middleware::from_fn(security_headers));
    router.with_state(app)
}
