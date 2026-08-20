# 开发计划：项目级访问令牌（project-token）

日期: 2026-08-20
上游: docs/design/project-token.md（已审核通过）
执行路线: 单工作区逐 slice 实现，每 slice 后跑对应测试；全部完成后整体验证

## TDD Route

```text
TDD Route:
- Mode: off
- Decision: skipped
- Strict authority: not applicable（用户未要求 strict TDD）
- Test posture: post-change regression —— 既有测试随实现同步更新，新行为（token CRUD/项目隔离/吊销/哈希落盘/dev token）配对新测试
- Reason: 用户流程为「设计→计划→实现」，无 strict TDD 指令；仓库既有测试基线（cargo test --workspace）即回归网
- Verification: cargo test --workspace + scripts/api-surface-test.sh + 手动 UI 清单
```

## 0. 目标与基线

- 目标（见设计文档 §1）：① 数据面鉴权改为每项目 token 集合（多 token 并存、可独立吊销、轮换零中断）；② 彻底移除 --data-plane-token 全局令牌，数据面默认关闭；③ 管理面 CRUD（仅全局管理员）+ Admin UI；④ --dev-single 自动生成开发 token 打印。
- 兼容边界：breaking change（删除 --data-plane-token；数据面默认要求项目 token）；**不做数据迁移**（token 为新增状态，无旧数据；升级即断点，迁移顺序见 §9.6）。
- 基线命令：`cd server && source ../scripts/build-env.sh && cargo test --workspace`（基线 172 测试，实现后全绿 + 新增）。
- 二进制：`server/target/debug/defing`；e2e：`bash scripts/api-surface-test.sh`（自起 dev-single）。

## 1. 文件地图

| 文件 | 动作 | 内容 |
| --- | --- | --- |
| server/crates/dsh-core/src/model.rs | 改 | +ProjectTokenRecord |
| server/crates/dsh-core/src/keys.rs | 改 | +K_DATA_TOKEN / data_token_key |
| server/crates/dsh-core/src/command.rs | 改 | +ProjectTokenCreate / ProjectTokenRevoke |
| server/crates/dsh-core/src/state.rs | 改 | apply 分支 + 两个 apply 函数 + get_data_token / list_project_tokens + 项目删除级联 token |
| server/crates/dsh-core/src/lib.rs | 改 | 导出 ProjectTokenRecord |
| server/crates/dsh-core/tests/data_token.rs | 增 | token 状态机测试 |
| server/crates/dsh-api/src/lib.rs | 改 | ApiState 去 data_plane_token 加 dev_token；管理面 3 handler + 路由；pa_allowed 拒 /tokens；HTTP 数据面按项目鉴权 + 3 个 helper |
| server/crates/dsh-api/src/grpc.rs | 改 | 删 data_plane_interceptor；handler 内 authorize_project / authorize_data_plane |
| server/crates/dsh-api/tests/grpc_data_plane.rs | 改 | 去 interceptor，改项目 token 种子 |
| server/crates/dsh-api/tests/http_project_token.rs | 增 | 管理面 tokens 端点用例 |
| server/crates/dsh-cli/src/main.rs | 改 | 删 --data-plane-token；dev-single 生成 dev token 打印；spawn_grpc 去 interceptor；with_retention 参数 |
| server/crates/dsh-api/admin/index.html | 改 | 项目页 +「访问令牌」Tab + 创建成功弹窗 |
| server/crates/dsh-api/admin/app.js | 改 | tokens 渲染/创建/吊销/权限显隐 |
| api/openapi.v1.yaml | 改 | +3 端点 + ProjectToken schema |
| schema/storage.v1.schema.json | 改 | +ProjectTokenRecord |
| scripts/api-surface-test.sh | 改 | +token 流程 |
| scripts/sdk-contract-test.sh | 改 | 先建 token 再跑三语言 |
| README.md | 改 | 快速开始（dev token） |
| docs/deployment-guide.md | 改 | §3.5/§9/§10 |
| docs/design-modules/05-api.md | 改 | 数据面鉴权说明 |
| docs/design-modules/12-sdk.md | 改 | token 说明 |

## 2. Slice 划分

- S1 dsh-core（model/keys/command/state + tests）→ cargo test -p dsh-core
- S2 管理面 API（lib.rs handlers/routes/权限 + 新测试）→ cargo test -p dsh-api
- S3 数据面鉴权（lib.rs HTTP 中间件 + grpc.rs + grpc_data_plane.rs）→ cargo test -p dsh-api
- S4 CLI（main.rs 删 flag + dev token + spawn_grpc）→ cargo build
- S5 Admin UI（index.html/app.js）→ 手动 UI 清单 + cargo build
- S6 契约与脚本（openapi/storage schema/api-surface-test/sdk-contract-test/README/deployment-guide/design-modules）→ bash scripts/api-surface-test.sh
- S7 全量验证（cargo test --workspace + e2e + 升级演练）

## 3. S1：dsh-core

### 任务 3.1 model.rs：+ProjectTokenRecord（放在 ProjectAdminAccount 定义之后）

```rust
/// 项目访问令牌（机器凭据）：数据面鉴权用；明文仅在创建响应出现一次，
/// 落盘只存 SHA-256（token_hash）。key = tok/{hash}（扁平，鉴权单次 KV 读）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectTokenRecord {
    /// token id（= hash 前 16 位 hex），集群内唯一。
    pub id: String,
    /// 展示名（如 "订单服务 2025-08"），项目内唯一（校验 [A-Za-z0-9._-]{1,64}）。
    pub name: String,
    /// 所属项目（鉴权时校验请求项目 == 记录项目）。
    pub project: ProjectId,
    /// SHA-256(明文 token) hex —— 落盘/备份/审计永无明文。
    pub hash: String,
    pub created_at: u64,
    /// 创建人（principal_op 输出："admin" / "pa:{username}"）。
    pub created_by: String,
    /// 软删除标记（数据面鉴权过滤；保留记录供审计追溯）。
    pub revoked: bool,
}
```

验证：`cargo test -p dsh-core`（编译通过）。

### 任务 3.2 keys.rs：+K_DATA_TOKEN / data_token_key

```rust
/// 项目访问令牌键：tok/{hash}（扁平；数据面鉴权单次 KV 读）。
pub const K_DATA_TOKEN: &str = "tok/";
pub fn data_token_key(hash: &str) -> String {
    format!("{K_DATA_TOKEN}{hash}")
}
```

（keys.rs 底部 tests 追加断言 `assert_eq!(data_token_key("ab12"), "tok/ab12");`。）

### 任务 3.3 command.rs：+2 变体（追加到枚举末尾 GrayAbort 之后）

```rust
    // ---------------- 项目访问令牌（project-token，纯新增变体，既有变体不动） ----------------
    /// 创建项目访问令牌：校验项目存在、name 项目内唯一；只落 SHA-256 hash（明文不落库/不落日志）。
    ProjectTokenCreate {
        project: ProjectId,
        name: String,
        token_hash: String,
        #[serde(default)]
        operator: String,
        /// 墙钟 ms（API 层注入；0 = 回退 apply 的 now_ms 参数，旧日志重放兼容）
        #[serde(default)]
        ts: i64,
    },
    /// 吊销项目访问令牌（软删除：revoked=true；重复吊销幂等）。
    ProjectTokenRevoke {
        project: ProjectId,
        token_id: String,
    },
```

### 任务 3.4 state.rs：apply 分支（apply_inner 末尾，GrayAbort 分支之后）

```rust
            Command::ProjectTokenCreate {
                project,
                name,
                token_hash,
                operator,
                ts,
            } => self.apply_project_token_create(
                project,
                name,
                token_hash,
                operator,
                Self::eff_ts(ts, now_ms),
            ),
            Command::ProjectTokenRevoke { project, token_id } => {
                self.apply_project_token_revoke(project, token_id)
            }
```

### 任务 3.5 state.rs：apply 实现 + 访问器（放在 apply_project_admin_* 附近；顶部 imports 补 `use crate::keys::{data_token_key, K_DATA_TOKEN};` 与 model 的 ProjectTokenRecord）

```rust
    /// token 名称字符集：[A-Za-z0-9._-]{1,64}。
    fn valid_token_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    }

    fn apply_project_token_create(
        &mut self,
        project: &ProjectId,
        name: &str,
        token_hash: &str,
        operator: &str,
        now_ms: i64,
    ) -> ApplyOutcome {
        if !Self::valid_token_name(name) {
            return Err(Error::new(ErrorKind::Validation, "token 名称须为 1-64 位 [A-Za-z0-9._-]"));
        }
        if load::<Project>(&*self.store, &project_key(project))?.is_none() {
            return Err(Error::new(ErrorKind::NotFound, format!("项目 {project} 不存在")));
        }
        // 幂等：同 hash（同一明文 token）已存在 → no-op（重试/重放安全）
        if load::<ProjectTokenRecord>(&*self.store, &data_token_key(token_hash))?.is_some() {
            return Ok(vec![]);
        }
        // name 项目内唯一（扫 tok/ 前缀过滤项目，O(全部 token 数)，创建低频可接受）
        for (_, raw) in self.get_prefix_merged(K_DATA_TOKEN.as_bytes())? {
            if let Ok(rec) = serde_json::from_slice::<ProjectTokenRecord>(&raw) {
                if rec.project == *project && rec.name == name {
                    return Err(Error::new(ErrorKind::Conflict, "该项目下 token 名称已存在"));
                }
            }
        }
        // operator 空串（旧日志）按命令.rs 约定归一为 "admin"
        let created_by = if operator.is_empty() { "admin" } else { operator };
        let id: String = token_hash.chars().take(16).collect();
        let rec = ProjectTokenRecord {
            id,
            name: name.to_string(),
            project: project.clone(),
            hash: token_hash.to_string(),
            created_at: now_ms.max(0) as u64,
            created_by: created_by.to_string(),
            revoked: false,
        };
        self.save_pending(&data_token_key(token_hash), &rec)?;
        Ok(vec![])
    }

    fn apply_project_token_revoke(&mut self, project: &ProjectId, token_id: &str) -> ApplyOutcome {
        // 按项目 + id 定位（扫 tok/ 前缀；吊销低频可接受）
        let mut target: Option<Vec<u8>> = None;
        for (k, raw) in self.get_prefix_merged(K_DATA_TOKEN.as_bytes())? {
            if let Ok(rec) = serde_json::from_slice::<ProjectTokenRecord>(&raw) {
                if rec.project == *project && rec.id == token_id {
                    target = Some(k);
                    break;
                }
            }
        }
        let Some(key) = target else {
            return Err(Error::new(ErrorKind::NotFound, "token 不存在"));
        };
        let key_str = String::from_utf8_lossy(&key).to_string();
        let Some(mut rec) = load::<ProjectTokenRecord>(&*self.store, &key_str)? else {
            return Err(Error::new(ErrorKind::NotFound, "token 不存在"));
        };
        if rec.revoked {
            return Ok(vec![]); // 幂等
        }
        rec.revoked = true;
        self.save_pending(&key_str, &rec)?;
        Ok(vec![])
    }

    /// 数据面鉴权：按 hash 读 token 记录（O(1) 单次 KV 读）。
    pub fn get_data_token(&self, hash: &str) -> Result<Option<ProjectTokenRecord>, Error> {
        self.load_merged(&data_token_key(hash))
    }

    /// 管理面列表：某项目全部 token（含已吊销；按创建时间升序）。
    pub fn list_project_tokens(&self, project: &ProjectId) -> Result<Vec<ProjectTokenRecord>, Error> {
        let mut out = vec![];
        for (_, raw) in self.get_prefix_merged(K_DATA_TOKEN.as_bytes())? {
            if let Ok(rec) = serde_json::from_slice::<ProjectTokenRecord>(&raw) {
                if rec.project == *project {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }
```

验证：`cargo test -p dsh-core`（编译）。

### 任务 3.6 state.rs：apply_project_delete 级联 token（1246-1250 行项目管理员级联循环之后追加；注意：token 是扁平 tok/ 前缀，不在 p/{pid} 下，必须显式清理）

```rust
        // 级联删除该项目全部访问令牌（扁平 tok/ 前缀过滤项目）
        for (k, raw) in self.get_prefix_merged(K_DATA_TOKEN.as_bytes())? {
            if let Ok(rec) = serde_json::from_slice::<ProjectTokenRecord>(&raw) {
                if rec.project == *id {
                    self.delete_pending(&k)?;
                }
            }
        }
```

### 任务 3.7 lib.rs 导出：pub use model 列表追加 ProjectTokenRecord（字母序放 ProjectAdminAccount 后）

### 任务 3.8 新测试 server/crates/dsh-core/tests/data_token.rs（参照 tests/project_admin.rs 的构造方式）

```rust
use dsh_core::command::Command;
use dsh_core::model::{ProjectId, ProjectTokenRecord};
use dsh_core::state::StateMachine;
use dsh_core::store::InMemoryStore;
use dsh_core::{token_hash, ErrorKind};

fn sm() -> StateMachine {
    StateMachine::new(Box::new(InMemoryStore::new()))
}

fn seed_project(s: &mut StateMachine) {
    s.apply(&Command::ProjectCreate { name: "p".into(), operator: String::new(), ts: 0 }, 1).unwrap();
    s.apply(&Command::ProjectCreate { name: "q".into(), operator: String::new(), ts: 0 }, 2).unwrap();
}

fn create(s: &mut StateMachine, project: &str, name: &str, raw: &str) {
    s.apply(&Command::ProjectTokenCreate {
        project: project.into(),
        name: name.into(),
        token_hash: token_hash(raw),
        operator: "admin".into(),
        ts: 0,
    }, 10).unwrap();
}

#[test]
fn create_stores_hash_only() {
    let mut s = sm();
    seed_project(&mut s);
    let raw = "abc123def456";
    create(&mut s, "p", "svc-a", raw);
    let rec = s.get_data_token(&token_hash(raw)).unwrap().unwrap();
    assert_eq!(rec.project.0, "p");
    assert_eq!(rec.name, "svc-a");
    assert_eq!(rec.hash, token_hash(raw));
    assert_ne!(rec.hash, raw);          // 无明文
    assert_eq!(rec.id.len(), 16);        // id = hash 前 16 位
    assert!(!rec.revoked);
    assert_eq!(rec.created_by, "admin");
}

#[test]
fn create_rejects_missing_project() {
    let mut s = sm();
    seed_project(&mut s);
    let e = s.apply(&Command::ProjectTokenCreate {
        project: "nope".into(),
        name: "x".into(),
        token_hash: token_hash("t"),
        operator: String::new(),
        ts: 0,
    }, 10).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
}

#[test]
fn create_rejects_dup_name_in_project() {
    let mut s = sm();
    seed_project(&mut s);
    create(&mut s, "p", "svc-a", "raw1");
    let e = s.apply(&Command::ProjectTokenCreate {
        project: "p".into(),
        name: "svc-a".into(),
        token_hash: token_hash("raw2"),
        operator: String::new(),
        ts: 0,
    }, 11).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Conflict);
}

#[test]
fn same_name_ok_in_other_project() {
    let mut s = sm();
    seed_project(&mut s);
    create(&mut s, "p", "svc-a", "raw1");
    create(&mut s, "q", "svc-a", "raw2");   // 不冲突
    assert!(s.get_data_token(&token_hash("raw2")).unwrap().is_some());
}

#[test]
fn create_idempotent_same_hash() {
    let mut s = sm();
    seed_project(&mut s);
    create(&mut s, "p", "a", "same");
    create(&mut s, "p", "b", "same");   // 同明文 → no-op
    let list = s.list_project_tokens(&ProjectId("p".into())).unwrap();
    assert_eq!(list.len(), 1);
}

#[test]
fn revoke_isolation_and_idempotent() {
    let mut s = sm();
    seed_project(&mut s);
    create(&mut s, "p", "a", "raw-p");
    create(&mut s, "q", "b", "raw-q");
    // p 的 token 不能从 q 项目吊销
    let e = s.apply(&Command::ProjectTokenRevoke {
        project: "q".into(),
        token_id: "0000000000000000".into(),
    }, 20).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    // 正确吊销
    let id = s.get_data_token(&token_hash("raw-p")).unwrap().unwrap().id;
    s.apply(&Command::ProjectTokenRevoke { project: "p".into(), token_id: id.clone() }, 21).unwrap();
    let rec = s.get_data_token(&token_hash("raw-p")).unwrap().unwrap();
    assert!(rec.revoked);
    // 重复吊销幂等
    s.apply(&Command::ProjectTokenRevoke { project: "p".into(), token_id: id }, 22).unwrap();
}

#[test]
fn project_delete_cascades_tokens() {
    let mut s = sm();
    seed_project(&mut s);
    create(&mut s, "p", "a", "raw-p");
    create(&mut s, "q", "b", "raw-q");
    s.apply(&Command::ProjectDelete { id: "p".into(), operator: String::new() }, 30).unwrap();
    assert!(s.get_data_token(&token_hash("raw-p")).unwrap().is_none());
    assert!(s.get_data_token(&token_hash("raw-q")).unwrap().is_some()); // q 不受影响
}
```

验证：`cargo test -p dsh-core --test data_token`，再 `cargo test -p dsh-core`（全绿）。

## 4. S2：管理面 API（dsh-api/src/lib.rs）

### 任务 4.1 ApiState：去 data_plane_token，加 dev_token

- struct 字段（66-67 行）替换为：

```rust
    /// dev-single 开发数据面 token（全局有效，仅 dev 模式注入；集群模式恒 None）
    dev_token: Option<std::sync::Arc<str>>,
```

- `with_retention` 末参 `data_plane_token: Option<std::sync::Arc<str>>` 改名为 `dev_token: Option<std::sync::Arc<str>>`（字段赋值同步改）；`new()` 已传 None，无需动。

### 任务 4.2 请求结构 + 3 个 handler（放 create_project_admin 附近；lib.rs 若无 rand 依赖需在 Cargo.toml 加 `rand = "0.8"`）

```rust
#[derive(Deserialize)]
struct CreateProjectTokenReq {
    name: String,
}

/// 数据面 token 明文生成（同 dsh-cli new_token：16B → 32 hex）。
fn new_token() -> String {
    let b: [u8; 16] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 创建项目访问令牌（仅全局管理员；PA 由 pa_allowed 拒 403，此处防御性再校验）。
async fn create_project_token(
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<CreateProjectTokenReq>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiErrorBody>)> {
    if !matches!(&*principal, dsh_core::Principal::Admin) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorBody {
                code: "ERR_FORBIDDEN".into(),
                message: "仅全局管理员可管理访问令牌".into(),
                detail: None,
            }),
        ));
    }
    let now = now_ms();
    let raw = new_token();
    let token_hash = dsh_core::token_hash(&raw);
    let cmd = Command::ProjectTokenCreate {
        project: ProjectId(pid.clone()),
        name: req.name.clone(),
        token_hash: token_hash.clone(),
        operator: principal_op(&principal),
        ts: now,
    };
    match app.write(&cmd, now).await {
        Ok(_) => {
            app.audit
                .append(
                    "token_create",
                    Some(pid.clone()),
                    Some(req.name.clone()),
                    None,
                    None,
                    serde_json::json!({}),
                    &principal_op(&principal),
                )
                .await;
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": token_hash.chars().take(16).collect::<String>(),
                    "name": req.name,
                    "token": raw,            // 明文仅此一次
                    "created_at": now,
                })),
            ))
        }
        Err(e) if e.0.kind == ErrorKind::Conflict => Err((
            StatusCode::CONFLICT,
            Json(ApiErrorBody {
                code: "ERR_TOKEN_NAME_EXISTS".into(),
                message: e.0.message.clone(),
                detail: e.0.detail.clone(),
            }),
        )),
        Err(e) if e.0.kind == ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorBody {
                code: "ERR_NOT_FOUND".into(),
                message: e.0.message.clone(),
                detail: e.0.detail.clone(),
            }),
        )),
        Err(e) => Err(e),
    }
}

/// 项目 token 列表（不含明文与 hash；含 revoked 标记）。
async fn list_project_tokens(
    State(app): State<ApiState>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiErrorBody>)> {
    let sm = app.sm.read().map_err(lock_err)?;
    let tokens = sm.list_project_tokens(&ProjectId(pid.clone())).map_err(ApiError::from)?;
    let out: Vec<serde_json::Value> = tokens
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "created_at": t.created_at,
                "created_by": t.created_by,
                "revoked": t.revoked,
            })
        })
        .collect();
    Ok(Json(serde_json::json!(out)))
}

/// 吊销项目 token（幂等；不存在 → 404）。
async fn delete_project_token(
    principal: axum::Extension<dsh_core::Principal>,
    State(app): State<ApiState>,
    AxumPath((pid, tid)): AxumPath<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorBody>)> {
    if !matches!(&*principal, dsh_core::Principal::Admin) {
        return Err((StatusCode::FORBIDDEN, Json(ApiErrorBody {
            code: "ERR_FORBIDDEN".into(),
            message: "仅全局管理员可管理访问令牌".into(),
            detail: None,
        })));
    }
    let now = now_ms();
    match app
        .write(
            &Command::ProjectTokenRevoke {
                project: ProjectId(pid.clone()),
                token_id: tid.clone(),
            },
            now,
        )
        .await
    {
        Ok(_) => {
            app.audit
                .append("token_revoke", Some(pid.clone()), Some(tid.clone()), None, None, serde_json::json!({}), &principal_op(&principal))
                .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) if e.0.kind == ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorBody { code: "ERR_NOT_FOUND".into(), message: e.0.message.clone(), detail: e.0.detail.clone() }),
        )),
        Err(e) => Err(e),
    }
}
```

### 任务 4.3 pa_allowed：/tokens 拒绝（544-549 行，与 /admins 并列）

```rust
        if path
            .strip_prefix(&format!("/api/v1/projects/{pid}"))
            .is_some_and(|r| r.starts_with("/admins") || r.starts_with("/tokens"))
        {
            return false;
        }
```

### 任务 4.4 路由（3800-3807 行 branches 附近）

```rust
        .route(
            "/api/v1/projects/{p}/tokens",
            get(list_project_tokens).post(create_project_token),
        )
        .route(
            "/api/v1/projects/{p}/tokens/{id}",
            delete(delete_project_token),
        )
```

### 任务 4.5 新测试 server/crates/dsh-api/tests/http_project_token.rs

（参照 http_project_admin.rs：起 dev-single 或 ApiState + Router 测试）用例：
- 创建：POST → 201，响应含 token 明文；GET 列表**无** hash/token 字段；
- 列表：GET → 数组含 id/name/revoked/created_at/created_by；
- 吊销：DELETE → 204；重复 DELETE → 204（幂等）；不存在 id → 404；
- 权限：项目管理员登录后 POST/GET/DELETE tokens → 403；
- 名称重复：同名再建 → 409。

验证：`cargo test -p dsh-api --test http_project_token`，再 `cargo test -p dsh-api`（既有用例不回归）。

## 5. S3：数据面鉴权改造

### 任务 5.1 lib.rs：数据面提取 + 鉴权 helper（放 project_segment 附近）

```rust
/// 从 /v1/projects/{p}/... 路径提取 {p}（数据面；字符集同 N2，非法 → None → 401）。
fn data_plane_project_segment(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v1/projects/")?;
    let seg = rest.split('/').next()?;
    if !seg.is_empty()
        && seg
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        Some(seg.to_string())
    } else {
        None
    }
}

/// 从请求提取数据面 token：Authorization: Bearer <t> 优先，其次 ?token=<t>（SSE EventSource 兼容）。
fn extract_data_token(req: &axum::extract::Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
    {
        return Some(v.to_string());
    }
    req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_string()))
    })
}

/// 数据面鉴权：dev token 全局有效；否则按项目校验（单次 KV 读）。
fn data_plane_authorized(app: &ApiState, req: &axum::extract::Request) -> bool {
    let Some(raw) = extract_data_token(req) else {
        return false;
    };
    if let Some(dev) = &app.dev_token {
        if raw == dev.as_ref() {
            return true;
        }
    }
    let Some(pid) = data_plane_project_segment(req.uri().path()) else {
        return false;
    };
    let sm = match app.sm.read() {
        Ok(s) => s,
        Err(_) => return false,
    };
    match sm.get_data_token(&dsh_core::token_hash(&raw)) {
        Ok(Some(rec)) => !rec.revoked && rec.project.0 == pid,
        _ => false,
    }
}
```

### 任务 5.2 lib.rs：auth_middleware /v1/ 分支整体替换（609-642 行）

```rust
    } else if path.starts_with("/v1/") {
        // 项目访问令牌鉴权：/v1/projects/{p}/... 需该项目有效 token（Bearer 或 ?token=，
        // 后者兼容 SSE EventSource）；dev-single 开发 token 全局有效。无有效 token → 401。
        if !data_plane_authorized(&app, &req) {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ApiErrorBody {
                    code: "ERR_UNAUTHORIZED".into(),
                    message: "data-plane token required".into(),
                    detail: None,
                }),
            ));
        }
    }
```

### 任务 5.3 grpc.rs：删拦截器 + 加鉴权 helper

- 删除 `data_plane_interceptor` 函数（24-43 行）及顶部对应注释。
- 新增（放服务实现之前）：

```rust
/// 提取 metadata authorization Bearer。
fn metadata_bearer(meta: &tonic::metadata::MetadataMap) -> Option<String> {
    meta.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .map(|t| t.to_string())
}

/// 数据面鉴权（get_config/get_item/watch）：token 须属于该项目（dev token 全局有效）。
fn authorize_project(
    state: &ApiState,
    meta: &tonic::metadata::MetadataMap,
    project: &str,
) -> Result<(), Status> {
    let Some(raw) = metadata_bearer(meta) else {
        return Err(Status::unauthenticated("data-plane token required"));
    };
    if let Some(dev) = &state.dev_token {
        if raw == dev.as_ref() {
            return Ok(());
        }
    }
    let sm = state.sm.read().map_err(|_| Status::internal("sm lock"))?;
    match sm.get_data_token(&dsh_core::token_hash(&raw)) {
        Ok(Some(rec)) if !rec.revoked && rec.project.0 == project => Ok(()),
        _ => Err(Status::unauthenticated("invalid data-plane token")),
    }
}

/// 数据面鉴权（list_members：无 project 字段；任一有效项目 token 或 dev token 即放行）。
fn authorize_data_plane(state: &ApiState, meta: &tonic::metadata::MetadataMap) -> Result<(), Status> {
    let Some(raw) = metadata_bearer(meta) else {
        return Err(Status::unauthenticated("data-plane token required"));
    };
    if let Some(dev) = &state.dev_token {
        if raw == dev.as_ref() {
            return Ok(());
        }
    }
    let sm = state.sm.read().map_err(|_| Status::internal("sm lock"))?;
    match sm.get_data_token(&dsh_core::token_hash(&raw)) {
        Ok(Some(rec)) if !rec.revoked => Ok(()),
        _ => Err(Status::unauthenticated("invalid data-plane token")),
    }
}
```

### 任务 5.4 grpc.rs：handler 内鉴权

- `get_config` / `get_item` / `watch`：在 `let r = req.into_inner();` **之前**插入：

```rust
        let meta = req.metadata().clone();
        let r = req.into_inner();
        authorize_project(&self.state, &meta, &r.project)?;
```

  （`watch` 在流建立时校验一次，流生命周期内不重复校验。）
- `list_members`：

```rust
        let meta = req.metadata().clone();
        let _r = req.into_inner();
        authorize_data_plane(&self.state, &meta)?;
```

### 任务 5.5 grpc_data_plane.rs 测试重写

- `start_server` 去掉 token 参数与 `with_interceptor`，改 `ConfigServiceServer::new(ConfigGrpcService { state })`；
- 种子：测试内先 `apply(Command::ProjectTokenCreate { project: "p", name: "t", token_hash: dsh_core::token_hash("raw-token"), operator: String::new(), ts: 0 })`；
- 客户端统一带 metadata `authorization: Bearer raw-token`；
- 用例：正确 token → get/getItem/watch/listMembers 成功；无 token → Unauthenticated；错误 token / 他项目 token → Unauthenticated；吊销后 → Unauthenticated。

验证：`cargo test -p dsh-api --test grpc_data_plane`，再 `cargo test -p dsh-api`。

## 6. S4：dsh-cli（main.rs）

### 任务 6.1 删 --data-plane-token flag（137-139 行）

删除：

```rust
    /// 数据面 gRPC 访问令牌（metadata authorization: Bearer <token>；缺省开放，仅建议集群启用）
    #[arg(long)]
    data_plane_token: Option<String>,
```

### 任务 6.2 dev-single：生成 dev token 打印（795 行 admin_password 解析后）

```rust
        let admin_password = resolve_admin_password(&cli, "首次启动");
        let dev_token = new_token();
        eprintln!("开发数据面 token = {dev_token}（--dev-single 全局有效，可访问所有项目；集群模式无此机制）");
```

813 行 with_retention 调用：`cli.data_plane_token.clone().map(Arc::from),` →

```rust
            Some(Arc::from(dev_token.as_str())),
```

### 任务 6.3 集群模式（1060 行）

`cli.data_plane_token.clone().map(Arc::from),` → `None,`

### 任务 6.4 spawn_grpc（1083-1086 行）去 interceptor

```rust
    let svc = dsh_api::grpc::config_service_server::ConfigServiceServer::new(
        dsh_api::grpc::ConfigGrpcService { state },
    );
```

验证：`cd server && source ../scripts/build-env.sh && cargo build`（编译通过，无 data_plane_token 残留引用）。

## 7. S5：Admin UI（index.html / app.js）

### 任务 7.1 index.html：项目页 Tab + 面板

- pane-tabs（153-161 行）追加（默认隐藏，仅全局管理员显示，参照现有「管理员」导航的条件渲染模式）：

```html
              <button type="button" data-act="switchPane" data-pane="tokens" class="hidden" id="tab-tokens">访问令牌</button>
```

- 追加面板（versions pane 之后）：

```html
          <!-- 访问令牌 -->
          <div id="pane-tokens" class="pane hidden">
            <div class="card">
              <div class="card-head">
                <h3>访问令牌</h3>
                <div class="card-actions">
                  <button type="button" class="btn primary" data-act="createToken"><svg class="ic"><use href="#i-plus"/></svg>创建令牌</button>
                </div>
              </div>
              <p class="hint" style="margin:0 0 10px">数据面 SDK 凭据：仅全局管理员可管理；令牌仅创建时展示一次，泄露即吊销重建。</p>
              <div class="table-wrap">
                <table class="table">
                  <thead><tr><th>名称</th><th>ID</th><th>创建人</th><th>创建时间</th><th>状态</th><th></th></tr></thead>
                  <tbody id="tokens-body"></tbody>
                </table>
              </div>
            </div>
          </div>
```

- 新增创建成功弹窗（明文展示一次 + 复制按钮），参照现有 modal 结构：`#token-created-modal`（含 `<code id="token-plaintext">` 与复制按钮 `data-act="copyToken"`）。

### 任务 7.2 app.js：渲染 + 动作

- `switchPane` 分支加 `tokens`（切到该 pane 时调 `listTokens()`）；
- `listTokens()`：`GET /api/v1/projects/{p}/tokens` → 渲染 `#tokens-body`（revoked 显示「已吊销」徽标、吊销按钮禁用）；
- `createToken()`：输入名称 → `POST` → 把响应 `token` 明文填入 `#token-plaintext` 并打开 modal；
- `copyToken()`：`navigator.clipboard.writeText`；
- `revokeToken(id)`：confirm → `DELETE` → 刷新列表；
- 登录成功后按 `principal.kind === 'admin'` 显隐 `#tab-tokens`。

验证：`cargo build` + 手动 UI 清单（全局管理员：项目详情 → 访问令牌 Tab 创建/复制/吊销；项目管理员登录：无此 Tab）。

## 8. S6：契约与文档

### 任务 8.1 api/openapi.v1.yaml

+3 端点：`POST/GET /api/v1/projects/{p}/tokens`、`DELETE /api/v1/projects/{p}/tokens/{id}`；+`ProjectToken` schema（id/name/created_at/created_by/revoked，create 响应额外含 `token` 明文字段）；安全描述更新（数据面需项目 token）。

### 任务 8.2 schema/storage.v1.schema.json

+`ProjectTokenRecord`（id/name/project/hash/created_at/created_by/revoked）。

### 任务 8.3 scripts/api-surface-test.sh

在既有流程中追加：管理面创建 token → 带 token `GET /v1/projects/{p}/branches/dev/snapshot` 200 → 错 token 401 → 无 token 401 → 吊销后原 token 401。

### 任务 8.4 scripts/sdk-contract-test.sh

先经管理面为 sdk-project 建 token（curl 拿明文），以环境变量 `DSH_TOKEN` 传入三语言测试（TS/Go/Python 的 token 参数位）。

### 任务 8.5 README.md

- 快速开始 `--dev-single`：注明启动打印「开发数据面 token」；SDK 示例补 `{ token }`；
- 集群示例：注明需先为项目创建访问令牌。

### 任务 8.6 docs/deployment-guide.md

- §3.5：删 `--data-plane-token` 行，新增「项目访问令牌」小节（管理面 API/UI 创建；迁移顺序）；
- §9：SDK 示例补 token；
- §10：安全清单「数据面令牌」改「每项目配置访问令牌（仅全局管理员可管理）」。

### 任务 8.7 docs/design-modules/05-api.md + 12-sdk.md

数据面鉴权说明改为「每项目 token；Authorization Bearer 或 ?token=；dev 模式自动生成并打印」。

验证：`bash scripts/api-surface-test.sh`；`bash scripts/check-contracts.sh`。

## 9. 全量验证

1. `cd server && source ../scripts/build-env.sh && cargo test --workspace`（全绿，含新增用例）
2. `bash scripts/api-surface-test.sh`（含 token 流程）
3. `bash scripts/sdk-contract-test.sh`（三语言带 token）
4. `bash scripts/check-contracts.sh`
5. 手动 UI 清单（§7）
6. 升级演练：旧部署（带 --data-plane-token）→ 为每个项目建 token → 升级新版本 → 旧全局 token 401、项目 token 200（验证迁移顺序）

## 10. 风险

- **升级即断点**：删除 --data-plane-token 后无项目 token 的数据面 401 —— 迁移顺序（§8.6/§9.6）必须先行；
- **raft wire 兼容**：纯新增变体 + serde default，旧日志重放安全（multisession/gray 已有先例）；
- **dev token 语义**：仅 --dev-single 注入；集群模式恒 None，杜绝 dev 后门进生产；
- **token 明文泄露**：只存 SHA-256，创建响应一次；审计/备份/快照无明文（§3.8 测试断言）；
- **API 层 rand 依赖**：dsh-api 若无 rand 需在 Cargo.toml 新增（§4.2 注）；
- **PA 访问 tokens 端点**：pa_allowed 显式拒绝 + handler 防御性校验双保险（§4.2/4.3）；
- **list_members 鉴权**：无 project 字段，按「任一有效项目 token」放行（§5.3 authorize_data_plane）。
