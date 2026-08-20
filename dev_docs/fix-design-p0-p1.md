# 修复设计文档（P0–P2）—— 供评审，不含代码改动

> 版本：v1（评审稿） ｜ 日期：2025-08-16 ｜ 依据：dev_docs/deep-analysis-2025.md（F1–F20）
> 原则：① **wire/日志向后兼容**（旧 Raft 日志重放不破坏、旧客户端不破坏）；② **dev-single 与集群行为一致**；
> ③ **单一来源**（掩码/转发/事件构造收敛为公共函数，杜绝"修一处漏一处"）；④ 每项附验证手段。
> 编号：D- 前缀 = 设计项；F 编号与主报告一致。

---

## 0. 总览

| 优先级 | 设计项 | 关联缺陷 | 工作量 | 影响面 |
|--------|--------|----------|--------|--------|
| P0 | D-F1 HTTP watch 密文掩码（统一掩码函数） | F1 | 小 | dsh-watch/dsh-api/dsh-core |
| P0 | D-F2 branch_diff 密文掩码 | F2 | 极小 | dsh-api |
| P0 | D-F3 集群模式强制 join-token | F3 | 小 | dsh-cli/脚本/e2e |
| P0 | D-F6 集群写响应回填（R=WriteAck） | F6 | 中 | dsh-raft/dsh-publish/dsh-api/测试 |
| P0 | D-DKR Dockerfile 补 protoc | 部署 | 极小 | deploy/ |
| P1 | D-F9 secret 共享项只接受 string | F9 | 小 | dsh-api/validator |
| P1 | D-F7 redb 数据文件 0600 | F7a | 极小 | dsh-storage |
| P1 | D-F7b 轮换 KEK 自加密（可选） | F7b | 中 | dsh-crypto/command/raft |
| P1 | D-F4 节流键可信代理配置 + 转发透传 XFF | F4 | 小 | dsh-api |
| P1 | D-SDK Go ctx/HTTP watch 超时/TS 超时与声明/Python 退避与错误 | SDK | 中 | sdk/go、sdk/ts、sdk/python |
| P1 | D-UI 草稿编辑器类型化渲染 + 分支保持 + 401 重登 | UI | 小 | admin/index.html |
| P2 | D-F5 SSE 慢消费者关流、D-PRUNED 裁剪起点 snapshot_required | F5 | 小 | dsh-watch/dsh-api/grpc |
| P2 | D-TYPE 重放事件类型保真（VersionRecord 增字段） | 重放失真 | 小 | dsh-core/grpc/api |
| P2 | D-F8 转发统一 helper + 超时 | F8 | 小 | dsh-api |
| P2 | D-OPENAPI / D-STATUS / D-LOCK / D-JOIN / D-DEL / D-CSP / D-TEST / D-DOC | F10–F20 | 中 | 多处 |

---

## 1. P0 —— 必须修复

### D-F1 HTTP watch 密文掩码（F1）

**问题**：`/v1/projects/{p}/branches/{b}/watch`（SSE）无鉴权（`auth_middleware` 只覆盖 `/api/v1/`，
lib.rs:439），且 `watch_sse`（dsh-watch/src/lib.rs:57-87）直接 `serde_json::to_string(&PublishEvent)`，
changes 中 `Value::Secret` 完整序列化出 ciphertext（model.rs:137-140）。重放（lib.rs:2847-2863）
与实时（store.rs:561-564 广播）两条路径均泄漏。

**方案**：
1. **新增单一掩码函数**（收敛现存的 4 套掩码实现）——放 `dsh-core`（被 api/watch/render 共用）：
   ```rust
   // dsh-core/src/wire.rs（新文件）
   /// 脱敏后的 wire 值：Secret → {"type":"secret","masked":true}（不含任何密文字段）
   pub fn masked_wire_value(v: &Value) -> serde_json::Value { ... }
   /// 事件级脱敏：把 changes 中所有 Secret 替换为掩码形态（克隆，不改原事件）
   pub fn mask_event_for_wire(e: &PublishEvent) -> PublishEvent { ... }
   ```
   掩码形态建议 `{"type":"secret","masked":true}`（保留类型语义、无密文、带 masked 标记，
   与 gRPC 的 `masked: true` 语义对齐）。
2. **`watch_sse` 序列化前统一过 `mask_event_for_wire`**：重放与实时共用此唯一出口
   （`dsh-watch/src/lib.rs` 的 stream 构造处），一条路径修两处泄漏。
3. **连带**：`branch_diff`（D-F2）、`snapshot`、`render_config`（掩码分支）、共享列表改用
   `masked_wire_value`，删掉 `plain_value/plain_groups/apply_secret_policy/masked_shared_value`
   四套实现，只保留 reveal（解密）一条独立路径。

**备选**：仅在两处调用点分别打补丁（不收敛）——不推荐，正是本次泄漏的根因（多处掩码易漏）。

**兼容性**：SSE 事件 JSON 中 secret 的 `new_value` 形状变化（密文对象 → `{"type":"secret","masked":true}`）；
三语言 SDK 对 secret 本就按 `"***"`/掩码展示，形状兼容（SDK 解析 `any`，非 string 值直接展示，
建议同步把 masked 形态显示为 `***`）。

**验证**：新增测试——发布含 secret 项后订阅 `/watch`，断言事件 JSON 不含 `ciphertext`/`edek`/`nonce`/`ct`；
gRPC watch 对照不变。e2e：dev-single + 集群各一次。

### D-F2 branch_diff 密文掩码（F2）

**问题**：`lib.rs:946-950` 将 `Value::Secret(ct)` 原样放入 branch_a/branch_b；PA 可对自己项目调用
（`pa_allowed` 放行，lib.rs:400-417）。

**方案**：diff 值经 `masked_wire_value` 输出（secret → `{"type":"secret","masked":true}`）。
一处替换，复用 D-F1 的统一函数。

**验证**：新增测试——两分支含 secret 项，断言 `/diff` 响应无密文字段；PA 视角同断言。

### D-F3 集群模式强制 join-token（F3）

**问题**：`join_token_ok` 在 `join_token=None` 时恒 true（lib.rs:2359-2360）；CLI 默认 None
（main.rs:111-113）→ 默认部署 join 全开放（任意网络可达者可注册 learner 拉走全量日志）。

**方案**（推荐）：
1. **集群模式（`--node-id` 存在）启动时强制要求 `--join-token`**（缺失即启动报错退出，
   `main.rs` 装配处校验），dev-single 不要求（无 raft，join 端点无意义）；
2. 同步在集群模式要求 `--raft-token`（S5 同源问题），两者可共用一个值（文档说明）
   或各自独立；启动时若 raft-token 缺失给出醒目 warn（"raft RPC 未鉴权"）；
3. 全部演示/CI 脚本（cluster-demo.sh、chaos-test.sh、restart-test.sh、CI e2e job、
   docker-compose 3 节点）统一注入演示 token（如 `--join-token demo --raft-token demo`）；
4. README 快速开始补 join-token 示例。

**备选**：默认 deny + `--allow-insecure-join` 显式开关——更安全但改动脚本面更大，且与
"缺省不校验"的既有演示习惯冲突；采用推荐方案（强制 + 脚本更新）在演示与安全间平衡。

**兼容性**：行为破坏（集群模式新增必填参数）仅影响脚本与文档，README 同步更新；
旧数据目录重启不受影响（join-token 仅入集群引导路径）。

**验证**：不带 `--join-token` 启动集群节点 → 报错退出；带 token 的 join 成功；
无 token 请求 `/api/v1/cluster/join` → 401；全套 e2e 脚本回归。

### D-F6 集群写响应回填（F6）

**问题**：`write_command` 集群分支 `events: vec![]`（raft.rs:189-192），因 `TypeConfig::R =
Result<u64, Error>`（types.rs:33）只回传版本号；apply 时 events 其实已产生（store.rs:559-564）
仅被广播、未随响应返回。导致 publish 响应 `changes:[]`、structure publish `affected_branches:[]`、
shared publish `affected:[]`——dev-single 与集群行为不一致。

**方案**（推荐）：
1. **R 类型升级为 `WriteAck`**：
   ```rust
   // dsh-raft/src/types.rs
   #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
   pub struct WriteAck {
       pub version: u64,
       pub events: Vec<dsh_core::model::PublishEvent>,  // 本命令 apply 产出的事件（含 changes）
   }
   R = Result<WriteAck, dsh_core::Error>,
   ```
   store.rs:554-560 `resp = Ok(WriteAck { version: events.first().map(|e| e.version).unwrap_or(0), events })`；
2. `raft.rs` `client_write`/`try_client_write`/`write_command` 的返回类型相应改为
   `Result<WriteAck, Error>`；`write_command` 集群分支直接透传 `WriteAck`（不再 `events: vec![]`）；
3. `PublishService::publish/rollback/publish_structure` 删除"fallback 重读 active_version"逻辑，
   直接用 `wr.events`（消除并发写者下的读回竞态——现有 fallback 在并发写时可能读到更新的版本）；
4. 更新受影响的测试断言（cluster.rs/forward_hint.rs 中 `resp.data` 为 u64 的断言 → `.version`）。

**备选**：不改 R，apply 后在 API 层重读快照重算 diff——存在并发写竞态且 O(snapshot) 开销，不推荐。

**兼容性**：R 不进 Raft 日志（仅 client_write 响应体），旧日志重放不受影响；
R 需满足 openraft `AppDataResponse`（Serialize+Deserialize+Send，WriteAck 满足）；
集群测试断言需小幅更新（纯测试代码）。

**验证**：新增集群集成测试——3 节点下 publish，断言响应 `changes` 非空且与 dev-single 一致；
structure/shared publish 的 affected 列表非空。

### D-DKR deploy/Dockerfile 补 protoc（部署）

**问题**：`deploy/Dockerfile` builder 阶段 `cargo build --release -p dsh-cli` 依赖系统 protoc
（dsh-api/build.rs 用 tonic-prost-build），Dockerfile 未安装 → 官方 3 节点 compose 构建必失败
（docker-compose.local.yml 的 builder 装了 protobuf-compiler，可对照）。

**方案**：
1. builder 阶段加 `apt-get install -y --no-install-recommends protobuf-compiler`（对齐 local builder）；
2. 连带（同文件，小改）：`deploy/docker-compose.yml` 的 `--admin-password changeme` 改为
   `${DSH_ADMIN_PASSWORD:-changeme}`（O3 同类），并注入 join-token（对齐 D-F3）；
3. 文档注明 3 节点 compose 需先设置 DSH_ADMIN_PASSWORD。

**备选**：提交预生成 protobuf 代码并删 build.rs（更彻底但改动大，且生成代码版本漂移难管）；不推荐本轮。

**验证**：本机 `docker build -f deploy/Dockerfile .` 成功（若本机 Docker 可用）；CI 无法覆盖则
在文档标注手工验证步骤。

---

## 2. P1 —— 应该修复

### D-F9 secret 共享项只接受 string（F9）

**问题**：`write_shared_draft` 仅对 `Value::String` 加密（lib.rs:1103-1115）；`secret:true` +
int/json/bool 明文落共享草稿 → SharedPublish 级联进项目分支 → 数据面不掩码（非 Secret 变体）→
明文 secret 暴露。

**方案**（推荐）：**校验拒绝**——共享项 `secret:true` 时要求 `type == "secret"`（`ValueType::Secret`）
且值为字符串，否则 422（与结构项"secret 标志要求 secret 类型"规则一致，validator.rs:114-119）；
在 `write_shared_draft` 与 `apply_shared_draft_update`（state.rs:1127-1148）双层校验（API 层快失败
+ 状态机兜底防绕过）。

**备选**：加密任意类型值（JSON 序列化后加密，reveal 时反序列化还原类型）——语义更强但需扩展
Ciphertext 载荷携带明文类型，改动大；本轮不做，记为后续增强（D-SEC-TYPED）。

**验证**：新增测试——`secret:true, type:"int"` 的共享草稿被 422；string secret 正常加密；
级联后数据面掩码。

### D-F7a redb 数据文件 0600（F7a）

**问题**：`RedbStorage::open` 创建 `dsh.redb` 未设权限（默认 0644，dsh-storage/src/lib.rs:62-70），
文件含全部密文 + 密码哈希 + 会话哈希；S4 只修了 ring 文件。

**方案**：open 成功后 `std::fs::set_permissions(db_path, 0o600)`（复用 crypto save_ring 的
OpenOptions+set_permissions 模式，含 unix 权限断言测试）；对已存在文件同样 set（修复存量）。
可选：data_dir 目录权限校验（告警而非强制，避免破坏现有部署）。

**验证**：storage 测试新增 unix 权限断言（对齐 crypto 的 ring 0600 测试）。

### D-F7b 轮换 KEK 自加密（F7b，可选）

**问题**：`RotateMasterKey { kek }`（command.rs:189-192）明文随 Raft 日志复制到全部节点
（store.rs:347-351），DB 文件 0644 时日志即主密钥泄露面。

**方案**（可选，若本轮不做则先 D-F7a + 文档声明"raft 日志访问权 == 主密钥访问权"）：
命令载荷改自加密：`RotateMasterKey { kek_enc: Vec<u8> }`，`kek_enc = AEAD(old_kek, new_kek)`
（用当前 KEK 加密新 KEK）；apply 时各节点用自己 keyring 的当前 KEK 解密后入环。
- `#[serde(default)]` 保留旧 `kek` 字段兼容旧日志（旧日志仍是明文，属历史数据，不可逆）；
- 仅影响加密形态，轮换 API 面不变（new_key 输入 → 节点加密）。

**验证**：crypto 测试——自加密 roundtrip；轮换 e2e 回归（generation 递增、旧数据可解、重启可解）。

### D-F4 节流键可信代理配置 + 转发透传 XFF（F4）

**问题**：节流键取 X-Forwarded-For 首值且无可信代理配置（lib.rs:1846-1853）→ 伪造 IP 绕过节流 /
受害 IP 锁定 DoS；集群 login 转发不回传 XFF（lib.rs:1958-1962）→ leader 侧全记 "direct"，
5 次失败即集群级锁死直连登录。

**方案**：
1. 新增 `--trusted-proxy` 配置（`Option<String>`，CIDR/IP 列表）：未配置时**忽略 XFF**，
   节流键取对端 socket 地址（axum `ConnectInfo`，需在 serve 时 `.into_make_service_with_connect_info::<SocketAddr>()`）；
   配置时取 XFF（保留首值，但仅信任来自代理链的请求）；
2. login/rotate 转发请求携带原始 `X-Forwarded-For`（`x-forwarded-for: <原值>`），leader 侧按同规则取键；
3. 文档：集群多节点部署推荐前置 LB 并配置 --trusted-proxy。

**验证**：单测——无代理配置时伪造 XFF 不改变节流键；有配置时生效；
e2e——非 leader 登录失败 5 次后真实客户端被 429（跨节点转发路径）。

### D-SDK 三语言修复（SDK 严重/中危项）

**Go（优先级最高）**：
1. **gRPC ctx 贯通**：`g.ctx()` 改为 `ctx(ctx context.Context)` 接收调用方 ctx；Get/GetItem/
   ListMembers/Watch 签名加 ctx（`grpc_client.go:45-51,94-121,140`）；Watch 用 ctx 取消流
   （`ctx.Done()` + `stream.CloseSend()`）；同步更新 `grpc-test/main.go`；
2. **HTTP watch 独立 client**：SSE 用无整体 Timeout 的专用 `http.Client`
   （`ReadHeaderTimeout` 或仅 connect 超时），不再被 5s 掐断（`client.go:58,127`）；
3. `valueFromProto` default 分支改为显式返回未知类型错误或 `(nil, masked=true)`（grpc_client.go:70-72）。

**TS**：
1. `request()` 加 `AbortController` 超时（默认如 10s，可配，`index.ts:111`）；
2. `ensureGrpc()` 与 watch 内部 `import('./grpc.ts')` 补 `.catch`（index.ts:83,178-187）；
3. 声明修正：`package.json`/注释改"Node（gRPC）/浏览器（HTTP+SSE）"；grpc-js 移为 optionalDependency，
   HTTP-only 用户免装；提供构建产物（`tsc` 出 dist）或明确要求 TS bundler；
4. `listMembers` 返回 `Member[]`（index.ts:156-166）。

**Python**：
1. gRPC watch 退避改指数（`BACKOFF_BASE_MS * 2**n` 封顶 15s，config_client.py:184）；
2. `tls` 参数落实（https + ssl context）或删除并文档说明（config_client.py:57）；
3. 空端点列表构造时抛 `ConfigError`（config_client.py:62）；JSON 解析错误包装为 ConfigError。

**三语言共性**：
1. watch 回调按版本去重：`e.version <= lastEmitted` 直接跳过（重放/重连不重复投递）；
2. 新增契约测试：断线续传（重启服务器/断 TCP）、snapshot_required 慢消费者、裁剪后续传、
   事件字段断言（ty/request_id/structure_version）；ListMembers 真断言（dev-single 断言
   FailedPrecondition 状态码）。

**验证**：三语言契约测试增强后全绿；Go Watch cancel 测试（cancel 后 goroutine 退出）。

### D-UI 草稿编辑器类型化渲染 + 分支保持 + 401 重登（UI）

**问题**：L331-332 非 string 值显示 `[object Object]`、保存 `parseInt→NaN||0` 数据破坏；
bool checkbox 恒不勾选（判断 v 对象而非 v.bool_value）→ 保存即 false；L282 发布/回滚/提升后
分支重置到第一个；401 不引导重登。

**方案**：
1. **renderDraftEditor 按类型渲染**（index.html:329-332）：
   - `bool` → checkbox，checked 判定改 `v.bool_value === true`；
   - `int/float` → number input，`value="${v.int_value ?? v.float_value ?? ''}"`；
   - `json/array` → textarea（value = v.json_value / v.list_value.join(', ')）；
   - `secret` → password input + 显示 `***`（值不回显）；
   - 保存路径（L374-380）按 `data-ty` 用对应字段回读（checkbox 用 `inp.checked`，number 用
     `parseInt/parseFloat` 且校验 NaN 报错而非静默置 0）；
2. **分支保持**：`loadProject()` 保留当前 `curBranch`（仅当不存在于 `bs` 时才重置为 `bs[0]`，L282）；
3. **401 重登**：`j()` 收到 401 时清 TOKEN 并跳登录视图（L201-210）；
4. 顺带：EventSource 带 `after_version`（L437）、events 面板截断保留最近 200 条（L438）。

**验证**：浏览器自动化（或手测清单）——int/bool/json/array/secret 各类型回显与保存一致性；
prod 分支发布后停留 prod；token 失效后回登录页。

---

## 3. P2 —— 建议修复（简表）

| 设计项 | 问题 | 方案要点 |
|--------|------|----------|
| D-F5 | SSE 慢消费者静默丢事件、流不结束（dsh-watch/src/lib.rs:71-84） | `filter_map` 遇 `Err(Lagged)` 发一条 `snapshot_required` 事件（对齐 gRPC）后 `break` 关流；SDK 收到后重拉全量重订阅 |
| D-PRUNED | 断线起点被版本裁剪后静默丢事件（grpc.rs:192-227、lib.rs:2837-2869） | 重放前检测：`after_version > 0 && after_version < min(version_history) && after_version < active_version` → 先发 `snapshot_required` 事件；SSE/gRPC 双侧落地 |
| D-TYPE | 重放事件类型失真（结构发布/级联被标 value_publish） | `VersionRecord` 增 `#[serde(default)] event_ty: Option<EventType>`，apply 时落标；重放直接用，旧日志 default None → 按 rollback_of 推断（现状逻辑） |
| D-F8 | login/rotate 转发样板 ×3 且无超时 | 抽公共 `forward_to_leader(base, path, body, headers)` helper：reqwest builder connect 3s + total 10s + 透传 XFF（合并 D-F4）；三处调用替换 |
| D-OPENAPI | PA 账号路由 + /admin/{*path} 未登记 | openapi.v1.yaml 补 3 条路径 + Member/账号 schema |
| D-STATUS | LeaderRedirect 映射 409 语义混淆（lib.rs:172） | 改独立状态码（如 428）或保持 409 但响应体带 `leader_hint`（已有）并文档化；SDK 按 code 判断不受影响 |
| D-LOCK | ~20 处 expect("sm lock") | 统一改 `map_err → ApiError(Internal)`；apply 路径已用 map_err（store.rs:543-546），读路径跟进 |
| D-JOIN | join 无 node_id 唯一性/地址校验（lib.rs:2394） | 校验 node_id 未占用 + raft_addr/http_addr 可解析；D-F3 后风险已降，作为加固项 |
| D-DEL | update_draft deletes 无 `/` 条目静默丢弃（lib.rs:670-677） | 改为返回 Validation 错误（group/key 校验失败显式报错） |
| D-CSP | CSP 含 unsafe-inline（lib.rs:316-318） | Admin 页内联脚本外置为独立资源文件（rust-embed 多文件），移除 unsafe-inline；需同步改造 onclick → addEventListener/data-*（顺带消除 D-UI 的 L333 上下文转义隐患） |
| D-N2 | 版本保留默认 0（磁盘线性增长） | 保持现状（改动有数据删除风险），文档提示生产设置 --version-retention |
| D-TEST | 契约测试断言弱/ListMembers 零覆盖/无断线续传 | 见 D-SDK 共性 2；脚本 BIN 默认路径改仓库相对 + `pkill` 精确匹配 pidfile |
| D-DOC | 文档漂移（openapi snapshot 描述、ConfigSnapshot schema、proto SECRET 注释、progress 测试数） | 三处契约文档与实现对齐；progress/code-review 测试数更新为 123+ |

---

## 4. 实施顺序与依赖

1. **第一批（P0，可并行 3 个小项）**：D-F1 → D-F2 → D-F3 → D-DKR；
   D-F6 独立实施（类型改动，建议单独一次提交 + 全量测试）；
2. **第二批（P1）**：D-F9、D-F7a（小）；D-F7b、D-F4、D-SDK（中，可拆 Go/TS/Python 三个子任务）；
   D-UI 与 D-CSP 建议同批（都动 index.html）；
3. **第三批（P2）**：按 D-F5/D-PRUNED/D-TYPE（watch 可靠性一组）→ D-F8 → 其余低危项。
4. **每批结束**：`cargo fmt/clippy -D warnings/cargo test` 全绿 + 对应 e2e 脚本回归 +
   更新 dev_docs/code-review.md 修复状态表（沿用其第 8 节格式）。

---

## 5. 评审要点（请在评审中确认）

1. D-F6 的 R=WriteAck 方案是否接受（改 openraft 响应类型，测试断言小幅更新）；
2. D-F3 强制 join-token 的脚本改动面是否可接受（cluster-demo/chaos/CI/compose 全部注入 demo token）；
3. D-F7b 轮换自加密本轮做还是延后（若延后，接受"日志访问权=密钥访问权"的文档化声明）；
4. D-SDK 是否引入不兼容变更（Go Watch 签名加 ctx 属 breaking change，SDK 版本号需 bump）；
5. D-UI/D-CSP 合批改造的排期。
