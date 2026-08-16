# 研究：去掉单会话锁的可行性 —— 写性能与测试便利性分析

> 日期：2025-08-16 ｜ 依据：生产环境实测（Alibaba Cloud Linux 3）暴露的单会话阻塞问题 + 代码级机制分析
> 结论先行：**去掉单会话锁对写性能无直接提升，但显著提升测试/运维便利性，且不削弱写安全**——
> 因为单会话锁本就不保护写安全（写安全由 Raft 单写者 + 幂等键 + token 认证保证）。建议改为"多会话并存 + 每会话独立管理"。

---

## 1. 问题背景（生产实测暴露）

3 节点集群基准期间，单会话机制反复阻断操作：
- 登录返回 **409 `ERR_SESSION_IN_USE`**（已有会话在线）→ token 获取失败；
- 每次基准脚本重跑前必须**重启整个集群清会话**（state.rs:1773-1774 判定 `get_session().is_some()`）；
- 单节点/集群基准共经历 6+ 次"409 → 重启"循环，严重拖慢验证流程。

## 2. 单会话锁到底保护什么？（代码级）

### 2.1 单会话锁的机制

```rust
// state.rs:1773-1774 —— 管理员登录
if self.get_session()?.is_some() {
    return Err(Error::new(ErrorKind::SessionInUse, "已有管理员在线"));
}
// state.rs:1887-1888 —— PA 登录（按 username 判 is_some）
```

会话 key：`sess/admin`（全局管理员单会话）、`sess/pa/{username}`（每 PA 账号单会话）。
**判定只查 is_some()，不读墙钟**（D16 确定性，apply 可重放）。

### 2.2 它保护的是"登录并发"，不是"写并发"

| 机制 | 保护对象 | 位置 |
|------|----------|------|
| **Raft 单写者**（client_write 串行） | **写安全**（状态机串行 apply，无并发写） | dsh-raft/src/raft.rs:100 |
| **request_id 幂等**（last_request_id） | **写安全**（重复提交不重复生效，I10） | state.rs:1226/1311 |
| **token 认证**（token_hash 比对） | **写安全**（身份校验，未授权不可写） | lib.rs resolve_principal |
| **审计**（operator 落库） | **可追溯** | observability |
| **单会话锁**（is_some 判定） | **登录并发管理**（同一账号只许一个在线会话） | state.rs:1773/1887 |

**结论：写安全完全由前三者保证，与单会话锁无关。** 单会话锁的语义是"同一管理员账号只允许一个 token 在线"（防多设备同时管理），而非"防止并发写"——并发写已被 Raft 天然串行化。

## 3. 去掉单会话锁对写性能的影响

### 3.1 写路径不经过会话锁

写命令（publish/draft/rollback/结构发布）的鉴权链：
```
Bearer token → resolve_principal（读会话 key，校验 token_hash）→ 授权矩阵 → app.write() → raft client_write
```
- `resolve_principal` 只**读**会话 key（`sm.read()`，方案③后并发读）；
- 写本身走 Raft 串行，与会话数量无关；
- **会话锁不参与写路径的互斥**。

### 3.2 实测印证

生产基准中，单会话（85 QPS）与集群（11 QPS）的写吞吐差异完全由 fsync/网络复制解释，
**与"是否允许多会话"零相关**。因此：

> **去掉单会话锁 → 写性能无直接变化**（写瓶颈是 fsync 次数 × Raft 复制，非会话管理）。

### 3.3 间接收益（真实存在）

- **测试/运维便利性大幅提升**：不再需要"重启清会话"（本次生产验证的 6+ 次重启归零）；
- 多测试脚本可并行登录（CI 友好）；
- 减少 409 重试逻辑（SDK/脚本侧）。

## 4. 替代机制设计（多会话并存 + 每会话独立管理）

### 4.1 状态模型

```rust
// 现状：sess/admin → AdminSession（单 key 覆盖）
// 目标：sess/admin/{session_id} → AdminSession（每 token 独立 key）
//       sess/pa/{username}/{session_id} → AdminSession

AdminSession {
    token_hash, issued_at, expires_at, device_id, principal,
    session_id: String,   // 新增：随机 32B hex，随 token 返回
}
```

- `session_id` 由 API 层生成（不进状态机判定，只作 key 的一部分 + 返回给客户端）；
- **key 含 session_id** → 同账号多 token 并存，互不覆盖；
- 判定改为"按 token 定位到精确 key"（`resolve_principal` 的 token 前缀路由已支持：`adm.{session_id}.{secret}`）。

### 4.2 会话管理语义（替换单会话锁）

| 操作 | 现状（单会话） | 目标（多会话） |
|------|--------------|----------------|
| 登录 | 已有会话 → 409 | **始终成功**，新建会话 key |
| 登出 | 删 sess/admin | 删 `sess/admin/{session_id}`（仅自己） |
| 心跳 | 续期 sess/admin | 续期自己的 key |
| force-logout | 踢全局/指定 PA | 可**批量**踢（前缀扫描 `sess/admin/` 或 `sess/pa/{u}/`）或按 session_id 踢单个 |
| 过期清理 | 登录时重登组合 | 后台任务按 expires_at 清理孤儿 key |

### 4.3 写安全保持（不退化）

- **Raft 单写者**：不变（状态机串行）；
- **request_id 幂等**：不变（防重复提交）；
- **token 认证**：不变（每个 token 独立 hash，校验更精确）；
- **审计**：不变（operator 贯穿）；
- **新增收益**：force-logout 可精确到单会话（`{session_id}`），运维更细粒度。

### 4.4 兼容性（Raft wire）

- 新命令变体：`SessionLogin` 加 `session_id` 字段（`#[serde(default)]` 空串 → 旧行为单会话）；
  或纯新增 `MultiSessionLogin` 变体（更保守，旧日志重放安全）；
- 旧会话 key（`sess/admin`）兼容：resolve_principal fallback 到旧 key；
- **集群升级纪律**（与 PA 功能一致）：先全集群升级，再启用多会话。

## 5. 测试便利性量化

| 场景 | 现状（单会话） | 目标（多会话） |
|------|--------------|--------------|
| 基准脚本重跑 | 需重启服务清会话 | **直接重跑**（登录恒成功） |
| 多脚本并行 | 409 冲突 | 各自会话独立 |
| CI 集成测试 | 每用例重启 or force-logout | 每用例独立登录 |
| 生产运维 | 单会话被占→需踢 | force-logout 精确踢单会话 |

## 6. 结论与建议

1. **写性能**：去锁**无直接提升**（写瓶颈=fsync×Raft 复制，方案④/group commit 才是提升路径）；
2. **测试便利性**：**显著提升**（消除"409→重启"循环，本次生产验证直接受益）；
3. **写安全**：**不削弱**（Raft+幂等+认证保持；多会话反而支持更细粒度 force-logout）；
4. **建议**：作为**独立小项目**实施（1-2 天），改动集中在 dsh-core 会话命令 + dsh-api resolve_principal/login/force-logout；纯新增变体保 Raft wire 兼容；不影响本 roadmap 已验收的写优化。

## 7. 明确不做（本期）

- 不改写路径（写安全机制维持 Raft+幂等+认证）；
- 不做会话配额/账号限流（可后续叠加）；
- 不改变 PA 每账号单会话（如需多 PA 会话，同一机制扩展）。
