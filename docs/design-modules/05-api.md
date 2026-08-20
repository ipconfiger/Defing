# 模块 05 —— gRPC + HTTP 服务（dsh-api）

> 依据：design-v2 §5、api/openapi.v1.yaml、proto/config.v1.proto、design-v3 §7/§8
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：服务启动（tonic + axum 同一进程）、路由、鉴权中间件、幂等中间件、错误映射、
  proto ↔ 内部模型转换、渲染端点、健康检查端点。
- 不做：状态机业务（委托 core/publish）、事件扇出（委托 watch）。

## 2. 服务拓扑（单进程多端口）

```
trait dsh 进程
  ├─ tonic gRPC :8383（数据面；tonic-build 生成自 proto/config.v1.proto）
  └─ axum HTTP :8384（管理面 /api/v1、渲染 /v1/.../config、健康 /healthz /readyz、
                    Admin UI /admin 静态）
  ├─ Raft 内部 :8385（模块 03，不经 HTTP server）
```

## 3. 鉴权中间件

| 面 | 方案 |
|----|------|
| gRPC 数据面 | metadata `authorization: Bearer <项目访问令牌>`；只读+watch；per-handler 校验（按请求 project 查 tok/ 集合，SHA-256；dev-single 开发 token 全局有效） |
| HTTP 管理面 | Bearer 会话令牌；状态机 sess/admin 校验（I7）；登录/心跳免鉴权 |
| 渲染端点 | 项目访问令牌（与 snapshot 同构）；`reveal=true` 走会话鉴权（豁免 token，B2：PA 仅能 reveal 自己项目）+审计 |

## 4. 幂等中间件（I10）
- 管理写请求：读 `Idempotency-Key`（缺失则生成并回显）。
- 发布类：状态机 last_request_id 判定（模块 04）；CRUD 类：leader 内存窗口（10min）。
- 重放：重复请求返回首次响应（含同版本号），不重复生效。

## 5. 错误映射（ErrorKind → 对外）

| ErrorKind | gRPC status + code | HTTP status + body.code |
|-----------|--------------------|-------------------------|
| LeaderRedirect{hint} | FailedPrecondition + ERR_LEADER_REDIRECT | 409 / ERR_LEADER_REDIRECT + leader_hint |
| NotFound | NotFound + ERR_NOT_FOUND | 404 |
| Validation{detail} | InvalidArgument | 422 / ERR_VALIDATION |
| PublishBlocked{detail} | FailedPrecondition | 422 / ERR_PUBLISH_BLOCKED |
| VersionPruned | OutOfRange | 410 / ERR_VERSION_PRUNED |
| SessionInUse | Unauthenticated | 409 / ERR_SESSION_IN_USE |
| SessionExpired | Unauthenticated | 401 / ERR_SESSION_EXPIRED |
| Forbidden | PermissionDenied | 403 |
| CycleRef | InvalidArgument | 422 / ERR_CYCLE_REF |
| Conflict | Aborted | 409 / ERR_CONFLICT |
| NoDraft | FailedPrecondition | 409 / ERR_NO_DRAFT |
| LimitExceeded | ResourceExhausted | 429 / ERR_LIMIT_EXCEEDED |
| Internal/Storage/Raft/Crypto | Internal | 500 |

## 6. 关键 handler（节选，签名级）

```
// gRPC
async fn get_config(req: GetConfigRequest) -> Result<ConfigSnapshot>;   // 读：ReadIndex 后本地读 + proto 转换
async fn watch(req: WatchRequest) -> Result<WatchStream>;               // 委托模块 06
async fn list_members(_) -> Result<ListMembersResponse>;                // raft.metrics()
// HTTP（axum）
async fn publish(Path((p, b)), Json(PublishRequest)) -> Result<Json<PublishResult>>;
async fn rollback(...) -> Result<Json<RollbackResult>>;
async fn render_config(Path((p, b)), Query(RenderQuery)) -> Result<Response>;  // 模块 08 + Content-Type
async fn login(Json(LoginRequest)) -> Result<Json<LoginResponse>>;              // 单会话（I7）
```

## 7. proto ↔ 内部模型转换
- Value：proto oneof ↔ dsh_core::Value（secret：proto 传解密值 + masked 标记；存储层密文不跨 API）。
- 事件：PublishEvent ↔ proto WatchEvent（changes ↔ Change[]）。

## 8. 并发与资源
- tonic/axum 共享 tokio runtime；写路径统一走 Raft client（leader 转发由 raft 层处理）。
- 连接上限：gRPC 并发流限制、HTTP 请求体上限（`--max-body-size`，默认 4MB）。

## 9. 测试要点
- 契约 golden：proto/openapi/schema 三方 lint（design-v3 §8）；
- handler 单测（mock raft + 内存 storage）；错误映射全表测试；
- 鉴权/幂等/单会话集成测试（SDK-002 幂等重试）。

## 10. 任务清单
□ tonic-build + buf 生成 □ axum 路由与中间件栈（auth/idempotency/tracing）
□ gRPC handler（Get/GetItem/Watch/ListMembers） □ HTTP handler（§6 节选全量）
□ 错误映射表实现 □ 渲染端点（委托 08） □ 健康端点（委托 10）
□ 契约 golden 测试接入 CI
