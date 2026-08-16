# 设计文档：多会话并存 + 每会话独立管理（去单会话锁）

> 状态：待审核 ｜ 日期：2025-08-16 ｜ 依据：[research-multisession.md](../research-multisession.md)
> 目标：去掉"单会话锁"（同一账号仅一个 token 在线，登录 409 ERR_SESSION_IN_USE），
> 改为**多会话并存 + 每会话独立管理**——提升测试/运维便利性，不削弱写安全。
> 写安全不变：Raft 单写者 + request_id 幂等 + token 认证（与会话数量无关）。

---

## 1. 现状（代码证据）

| 项 | 现状 | 位置 |
|----|------|------|
| 管理员会话 | 单 key `sess/admin`；登录时 `is_some()` → 409 | state.rs:1773-1774 |
| PA 会话 | 单 key `sess/pa/{username}`；同账号 409 | state.rs:1887-1888 |
| token 格式 | `adm.{secret}`（管理员）；`pa.{username}.{secret}`（PA） | lib.rs:1933, 3238 |
| 登出/心跳 | 按 principal 定位单 key，无 session 粒度 | lib.rs:2305-2375 |
| force-logout | 踢全局（SessionLogout）或指定 PA（PaSessionLogout） | lib.rs:2812-2848 |
| 过期重登 | login 时 get_session 查过期 → 先 logout 再 login（N13） | lib.rs:2199-2221 |

**问题**：生产实测（3 节点集群基准）中，单会话导致"409 → 重启清会话"循环 6+ 次；多脚本/CI 无法并行登录。

## 2. 目标模型

```
会话存储（每 token 独立 key）：
  sess/admin/{session_id} → AdminSession
  sess/pa/{username}/{session_id} → AdminSession

token 格式（session_id 内嵌，O(1) 路由）：
  adm.{session_id}.{secret}          → sess/admin/{session_id}
  pa.{username}.{session_id}.{secret} → sess/pa/{username}/{session_id}

会话管理（每会话独立）：
  登录：始终成功（新建 key，不再 409）
  登出：删自己的 key（从 token 解析 session_id）
  心跳：续期自己的 key
  force-logout：踢全局全部会话 / 指定 PA 全部会话 / 指定 session_id 单个会话
```

## 3. 设计

### 3.1 dsh-core：命令扩展（Raft wire 兼容）

**原则**：既有命令变体加字段（`#[serde(default)]`），旧日志/旧客户端行为不变（空 session_id = 旧单会话语义）。

```rust
// command.rs —— 新增字段全部 #[serde(default)]，空串回退旧行为
SessionLogin {
    token_hash, issued_at, expires_at,
    #[serde(default)] session_id: String,   // 空 → 旧单会话（写 sess/admin）；非空 → 写 sess/admin/{sid}
}
SessionLogout {
    #[serde(default)] session_id: String,   // 空 → 删 sess/admin；非空 → 删 sess/admin/{sid}
}
SessionHeartbeat {
    expires_at,
    #[serde(default)] session_id: String,
}
PaSessionLogin {
    username, token_hash, issued_at, expires_at, device_id,
    #[serde(default)] session_id: String,
}
PaSessionLogout {
    username,
    #[serde(default)] session_id: String,   // 空 → 删该账号旧 key（兼容）；非空 → 删指定
}
PaSessionHeartbeat {
    username, expires_at,
    #[serde(default)] session_id: String,
}
```

**apply 语义**：
- `apply_session_login`：`session_id` 非空 → 写 `sess/admin/{sid}`（**不检查已存在**，多会话并存）；空 → 旧行为（`is_some()` → 409 单会话）；
- `apply_pa_session_login`：同理按 `session_id` 分支；
- `apply_session_logout` / `apply_session_heartbeat`：按 `session_id` 定位 key；空 → 旧 key 兼容；
- **级联**：`ProjectAdminSetPassword` / `ProjectAdminDelete` 级联删会话 → 改为前缀扫描删 `sess/pa/{username}/` 全部（批量）；`AdminSetPassword` 级联 → 删 `sess/admin/` 全部。

### 3.2 dsh-core：keys 函数

```rust
// keys.rs
pub fn session_key() -> &'static str { K_SESSION }                    // 旧：sess/admin（兼容）
pub fn session_key_with(sid: &str) -> String { format!("sess/admin/{sid}") }
pub fn pa_session_key(username: &str) -> String { format!("sess/pa/{username}") }  // 旧
pub fn pa_session_key_with(username: &str, sid: &str) -> String { format!("sess/pa/{username}/{sid}") }
pub const K_SESSION_PREFIX: &str = "sess/admin/";   // 批量操作前缀
```

### 3.3 dsh-api：token 与路由

```rust
// token 生成（lib.rs）
fn new_admin_token() -> String {
    let sid = new_session_id();          // 32B hex（new_token 复用）
    format!("adm.{sid}.{}", new_token())
}
fn new_pa_token(username: &str) -> String {
    format!("pa.{username}.{}.{}", new_session_id(), new_token())
}
fn new_session_id() -> String { new_token() }   // 随机 32B hex

// resolve_principal 路由扩展
// adm.{sid}.{secret} → sid = token.split('.')[1] → get_session_with(sid)
// adm.{secret}（无 sid，旧格式）→ fallback get_session()（旧 key）
// pa.{username}.{sid}.{secret} → get_pa_session_with(username, sid)
// pa.{username}.{secret}（旧）→ fallback get_pa_session(username)
```

**解析安全**：token 段数检查（`adm.` 后必须 ≥2 段；`pa.` 后必须 ≥3 段），段数不足按旧格式处理（hash 必败，无绕过面）。

### 3.4 dsh-api：login / logout / heartbeat / force-logout

- **login**：`session_id` 由 API 生成并写入命令；token 带 sid；**不再处理 409**（多会话始终成功）；N13 过期重登逻辑移除（无单会话冲突）；
- **logout**：从当前请求的 token 解析 sid（principal 已注入，可经 extensions 携带 sid）→ 命令带 sid 删自己的 key；
- **heartbeat**：同 logout，带 sid 续期自己的 key；
- **force-logout**：扩展请求体支持 `session_id`（可选）——指定则精确踢单个；否则按 username 前缀扫全部（新增命令 `AdminForceLogoutAll` 或复用 logout 变体批量）。

**Principal 扩展**：`axum::Extension<Principal>` 需携带 sid（写自己的会话用）。方案：扩展 `Principal::Admin { session_id }`？**不行**——Principal 序列化在会话中，改形状影响 Raft 数据兼容。
**替代**：sid 从 Authorization 头重新解析（logout/heartbeat handler 内解析一次，与 resolve_principal 同 helper），不改 Principal。

### 3.5 会话读取 API（dsh-core 访问器）

```rust
pub fn get_session_with(&self, sid: &str) -> Result<Option<AdminSession>, Error>;      // 新 key
pub fn get_pa_session_with(&self, username: &str, sid: &str) -> Result<Option<AdminSession>, Error>;
pub fn list_admin_sessions(&self) -> Result<Vec<AdminSession>, Error>;                 // 前缀扫（force-logout 全部/审计）
```

## 4. 影响面

| 位置 | 改动 |
|------|------|
| dsh-core command.rs | 6 命令加 `session_id` 字段（serde default） |
| dsh-core keys.rs | 4 个 key 函数 + 前缀常量 |
| dsh-core state.rs | apply 分支（按 sid）、级联批量删、访问器 |
| dsh-api lib.rs | token 生成、resolve_principal 路由、login/pa_login/logout/heartbeat/force-logout |
| dsh-core tests / dsh-api tests | 会话相关用例适配 + 多会话新用例 |

## 5. 测试计划

| 用例 | 断言 |
|------|------|
| T1 多会话并存 | 同账号连续登录 2 次 → 均成功，2 个 key 存在，2 个 token 各自有效 |
| T2 每会话独立登出 | logout(会话A) → A 失效、B 仍有效 |
| T3 每会话独立心跳 | heartbeat(会话A) → 仅 A 续期 |
| T4 force-logout 单个 | 指定 session_id → 仅该会话失效 |
| T5 force-logout 批量 | 踢账号全部 → 所有会话失效 |
| T6 旧格式兼容 | 无 sid 的旧 token → fallback 旧 key；旧日志（无 sid 字段）重放正常 |
| T7 改密/删号级联 | AdminSetPassword 后全部 admin 会话失效；PA 删号后其全部会话失效 |
| T8 写安全回归 | Raft+幂等+认证测试全绿（写安全不受影响） |
| T9 全量回归 | cargo test --workspace + e2e（dev-single/cluster/chaos/api-surface） |

## 6. 验收标准

1. `cargo test --workspace` 全绿（新增 T1-T7）；clippy/fmt 零告警；
2. e2e 4 脚本全过；
3. **多会话实测**：登录 2 次均成功（不再 409）；独立登出/心跳生效；
4. Raft wire 兼容：旧日志（无 session_id）重放不破坏；旧 token（无 sid）fallback 可用；
5. 写安全不削弱（T8 回归）。

## 7. 明确不做（本期）

- 会话配额/账号级限流（可后续叠加）；
- 会话列表 API（force-logout 用前缀扫即可，不暴露管理端点）；
- 不改写路径（Raft+幂等+认证保持）。

## 8. 审核修订记录（2025-08-16，子代理 Q1-Q6）

| # | 审核问题 | 处理 |
|---|---------|------|
| Q1 | token 路由安全（sid/secret 分隔、伪造、旧格式误判） | ✅ 设计正确：sid/secret 均 32 hex 无点号、hash 覆盖 token 全文（改 sid 即改 hash 输入必败）、旧格式段数不足不误判 |
| Q2 | **命令兼容（必修）**：给既有变体加字段 → 混合版本集群静默分叉 | ✅ **已修订**：既有变体全部恢复原状（SessionLogin/Logout/Heartbeat/Pa* 不动），**纯新增 8 个 Multi* 变体**（MultiSessionLogin/Logout/Heartbeat/LogoutAll、MultiPaSessionLogin/Logout/Heartbeat/LogoutAll）——遵循 project-admin.md §3.1/B1/N10 |
| Q3 | **级联双删（必修）**：前缀删漏旧格式单 key | ✅ **已实现**：`delete_all_pa_sessions`/`delete_all_admin_sessions` 旧 key + 前缀双删；project delete / pa delete / pa set_password / admin set_password 全部接入 |
| Q4 | Principal 不改形状（sid 经 Authorization 头重解析） | ✅ 实现：`token_session_id` helper 与 resolve_principal 同解析纪律（N15）；logout/heartbeat 加 HeaderMap 参数 |
| Q5 | force-logout 兼容性 | ✅ 实现：ForceLogoutReq 加 `session_id`（serde default）；四分支（单 admin 会话/单 PA 会话/PA 全部/admin 全部） |
| Q6 | 无过期会话回收（建议） | ⚠️ **风险接受**：功能安全由 resolve_principal 的 not_expired 判定保证（过期 token 立即失效）；残留 key 仅少量存储 + 登录节流限制累积速率；不引入 GC job（避免过度设计），文档化 |

**实现状态**：开发完成。dsh-core（8 Multi 命令 + keys + apply + 级联双删 + 访问器）、dsh-api（token 生成/resolve_principal 路由/logout/heartbeat/force-logout/login/pa_login）、测试（T1-T7 新增 + 2 个旧测试适配多会话语义 + cluster-demo 断言适配）。**实测**：连续登录 3 次全成功（不再 409）、会话独立登出/心跳、force-logout 批量全清；cargo test 31 套件全绿、clippy/fmt 零告警、e2e（dev-single/cluster/chaos/api-surface）全过。
