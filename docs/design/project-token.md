# 设计文档：项目级访问令牌（Project Access Token）

状态: v1（设计评审已通过，待实现）
日期: 2026-08-20
范围: dsh-core / dsh-api / dsh-cli / Admin UI / SDK 文档 / 契约测试

## 1. 背景与目标

当前数据面鉴权是**单一全局静态 token**（`--data-plane-token`，metadata `authorization: Bearer <token>`，
未配置时数据面完全开放）。问题：

- **爆炸半径**：一个 token 泄露 → 整个集群所有项目的配置全部暴露；
- **撤销粒度**：轮换全局 token 需同步所有客户端，无法按项目独立吊销；
- **无机器凭据的租户隔离**：人凭据（项目管理员账号）已是项目级，机器凭据（SDK token）仍是全局级。

目标：数据面鉴权改为**每项目 token 集合**（多 token 并存、独立创建/吊销、轮换零中断），
由全局管理员在管理面/Admin UI 管理；彻底移除全局 `--data-plane-token`，数据面默认关闭。

### 已确认的需求决策（与用户逐条确认）

| 决策点 | 结论 |
| --- | --- |
| token 管理权限 | **仅全局管理员**可创建/吊销项目 token（项目管理员 403） |
| 每项目 token 数量 | **token 列表**（多 token 并存，轮换 = 新建 + 旧的下次发版后吊销，零中断） |
| token 权限范围 | **项目级只读**（该项目所有分支的 snapshot/watch；不区分分支） |
| 全局 `--data-plane-token` | **彻底移除**（无集群兜底、无"未配置开放"；数据面默认关闭） |
| token 过期 | **永不过期**，管理靠主动吊销 |
| dev 模式（`--dev-single`） | 启动自动生成**一个全局开发 token** 并打印（可访问所有项目，仅 dev 模式） |

### 非目标（YAGNI，留升级缝）

- 分支级 token：将来加可空 `branch_scope` 字段即可，不破坏存量；
- token 过期时间：将来加可空 `expires_at` 字段即可；
- 数据面写权限：数据面本就只读，不引入写作用域；
- SDK 签名/协议变化：token 仍走 `Authorization: Bearer`，SDK 零代码改动；
- 多租户计费/配额等运营能力。

## 2. 数据模型与存储

新实体 `ProjectTokenRecord`（dsh-core/model.rs，参照 `ProjectAdminAccount` 模式）：

```
ProjectTokenRecord {
  id: String,            // token id（= hash 前 16 位 hex），集群内唯一
  name: String,          // 展示名（如 "订单服务 2025-08"），项目内唯一（校验 [A-Za-z0-9._-]{1,64}）
  project: ProjectId,    // 所属项目（鉴权时校验请求项目 == 记录项目）
  hash: String,          // SHA-256(明文 token) hex —— 落盘/备份/审计永无明文
  created_at: u64,
  created_by: String,    // 创建人（全局管理员标识）
  revoked: bool,         // 软删除标记（保留记录供审计追溯；查询时过滤）
}
```

- **KV（扁平，hash 即 key）**：`tok/{hash}` → ProjectTokenRecord（keys.rs 新增 `K_DATA_TOKEN: "tok/"`）。
  - **数据面鉴权 = 单次 KV 读 O(1)**：请求带明文 token → SHA-256 → 直接 load `tok/{hash}` →
    校验未吊销且 `project` 匹配（`list_members` 无 project 字段 → 任一未吊销即放行）；
  - 项目 token 列表 / name 项目内唯一性校验走 `tok/` 前缀扫描（O(全部 token 数)，管理面低频操作，可接受）。
- **明文 token**：随机 32 hex（复用现有 `new_token()` 模式），**仅在创建响应中出现一次**；
  服务端只存哈希，无法回显明文 → UI 只提供"创建/吊销"，不提供"查看"。
- **命令**（raft wire 兼容：保持现有变体不动，纯新增，旧日志重放安全）：
  - `ProjectTokenCreate { project, name, token_hash, operator, ts }` → 校验项目存在、name 唯一；
  - `ProjectTokenRevoke { project, token_id, operator, ts }` → 幂等（吊销不存在 id 返回 NOT_FOUND）。

## 3. 管理面 API（仅全局管理员）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/v1/projects/{p}/tokens` `{name}` | 创建 → 201 `{id, name, token(明文一次), created_at}` |
| GET | `/api/v1/projects/{p}/tokens` | 列表 `[{id, name, created_at, created_by, revoked}]`（**不含明文**） |
| DELETE | `/api/v1/projects/{p}/tokens/{id}` | 吊销（幂等；已吊销重复删 → 200/204） |

- **权限**：仅 `Principal::Admin`（全局管理员）；项目管理员 → 403（在授权矩阵默认拒绝，与项目管理员矩阵一致）。
- **审计**：`token_create` / `token_revoke` 事件（复用现有 `audit.append`，含 operator/项目/时间）。
- **Admin UI**：项目详情页新增「访问令牌」Tab —— 创建弹窗（展示明文一次 + 复制按钮）、
  列表（名称/创建人/时间/状态）、吊销确认。

## 4. 数据面鉴权改造

### 4.1 HTTP 数据面（/v1/projects/{p}/...）

- 中间件 `/v1/` 分支：先用新 helper 从路径提取项目（现有 `project_segment` 只解析
  `/api/v1/projects/`，需新增数据面变体，校验字符集同 N2），再查该项目 token 集合：
  - `Authorization: Bearer <token>` 或 `?token=<token>`（保留 SSE EventSource 兼容）；
  - 命中集合内任一 token 的 SHA-256 → 放行；否则 401 `ERR_UNAUTHORIZED`。
- 删除现有全局 `data_plane_token` 比对分支。

### 4.2 gRPC 数据面

- 鉴权从全局拦截器（`data_plane_interceptor`）**移到 handler 内**：
  - `get_config` / `get_item` / `watch`：读取请求体的 `project` 字段 → 查该项目 token 集合
    → 校验 metadata `authorization: Bearer <token>`；`Watch` 服务端流在**流建立时校验一次**，
    流生命周期内不再重复校验；
  - `list_members`：请求体为空（`ListMembersRequest {}`，无 project 字段），属集群级端点
    （SDK 端点池刷新用）→ 校验**任一有效项目 token**（或 dev 开发 token）即放行，
    不绑定具体项目。
- 删除 `data_plane_interceptor` 及 `--data-plane-token` CLI flag（dsh-cli/src/main.rs 137-139 行）
  与 `ApiState.data_plane_token` 字段。

### 4.3 dev 模式

- `--dev-single` 启动时：自动生成**一个全局开发 token**（随机 32 hex）打印到 stdout，
  复用 `--admin-password` 首启随机生成打印的模式；该 token 可访问所有项目（仅 dev 模式生效）。
- 所有模式（含 dev）数据面均需 token → README 快速开始/契约测试脚本同步更新。

## 5. SDK / 文档 / 迁移

- **SDK 零代码改动**：三语言 token 传递方式（Bearer header / gRPC metadata）不变；
  契约测试脚本需先经管理面建 token。
- **迁移（升级即断点，写入 deployment-guide）**：
  1. 升级前：对每个项目 `POST /api/v1/projects/{p}/tokens` 建 token，分发给各 SDK 客户端
     （环境变量/Secret 管理）；
  2. 客户端配置新 token；
  3. 升级服务端（移除全局 token 的版本）—— 升级后无有效项目 token 的数据面请求 401。
- **文档更新**：deployment-guide §3.5/§9/§10、README 快速开始、契约测试脚本、docs/design-modules/05-api.md 与 12-sdk.md 鉴权说明。

## 6. 测试

- **状态机单测**：create（项目不存在/name 重复/哈希落盘无明文）/ revoke（幂等/不存在）/
  项目隔离（A 项目 token 查不到 B 项目集合）。
- **鉴权矩阵**（HTTP + gRPC 各跑）：无 token / 错误 token / 他项目 token / 吊销后 /
  全局 token 已失效 / `?token=` 查询参数（HTTP）。
- **安全断言**：存储快照与导出中无任何 token 明文（全部 SHA-256）。
- **契约测试**：sdk-contract-test.sh 流程更新（先建 token），三语言全绿。
- **dev 模式**：`--dev-single` 打印开发 token 且数据面可用；`cargo test --workspace` 全绿。

## 7. 兼容边界与风险

- 升级即断点（§5）：旧版本部署无项目 token → 升级后数据面 401 —— 迁移顺序写入文档。
- raft wire 兼容：纯新增命令变体，旧日志重放安全（serde 缺省字段处理）。
- 性能：数据面鉴权从"全局一次比对"变为"按项目查集合 + 一次哈希"，集合小、O(1)，可接受。
- token 值永不过期（决策 5）：泄露即吊销是唯一回收途径 —— UI/API 必须有即时的吊销入口（已含）。
- dev 全局开发 token 仅 `--dev-single` 生效，绝不进入集群模式（集群模式无自动生成逻辑）。
