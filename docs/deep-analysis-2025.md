# Defing 分布式配置中心 —— 深度分析报告（v2 · 修复实施后）

> 分析日期：2025-08-16 ｜ 初始版基于 main `1a10242` 实测；**v2 为 F1–F20 全量修复实施后复核**
> （依据 [fix-design-p0-p1.md](fix-design-p0-p1.md)，实施记录见 [code-review.md](code-review.md) §8 第 4 轮）。
> 验证：`cargo test --workspace` **130 passed / 0 failed**、`cargo clippy --all-targets` 零告警、
> `cargo fmt --check` 干净、dev-single/api-surface(13 组)/cluster/chaos 四个 e2e 脚本全过、
> 集群轮换（共享主密钥）跨节点实测可解、HTTP watch 实测无密文。
> 维度：代码质量 / 完成度 / 安全性 / 功能闭环 / 竞品对比。
> 关联文档：[code-review.md](code-review.md)（S/C/R/N/O + F1–F20 修复状态）、
> [remaining-work.md](remaining-work.md)（D1–D5 已知偏差）、[progress.md](progress.md)。

---

## 0. 执行摘要

**总体结论：这是一个完成度高、工程纪律罕见地好的"毕业级"分布式配置中心原型，且经两轮安全审查 +
一轮全量修复后，对外安全承诺（数据面脱敏、join/raft 鉴权、主密钥保护）已基本闭环。** 12 个 Rust crate、
约 1.5 万行核心代码 + 3 千行三语言 SDK，契约三件套（proto/openapi/schema）、确定性状态机、
openraft 集群、内嵌 Admin UI、CI 全量流水线、混沌测试、SBOM 一应俱全。设计文档
（proposal→feasibility→design-v1/v2/v3→15 份模块规格→progress→code-review→deep-analysis）
构成完整的需求-设计-实现-审查-修复证据链，远超同类项目平均水准。

**v2 修复成果（本报告原 F1–F20 与部署缺陷，除明确标注外全部落地并实测验证）**：

| 原风险 | 修复后状态 |
|--------|-----------|
| 🔴 HTTP watch / branch_diff 泄漏 secret 密文（F1/F2） | ✅ 统一掩码函数（`dsh-core/wire.rs`），实测 watch 事件无密文字段 |
| 🔴 join/raft token 默认关闭（F3/S5） | ✅ 集群模式**强制** `--join-token`/`--raft-token`，无 token 启动即报错 |
| 🟠 集群写响应 changes 恒空（F6） | ✅ R 升级为 `WriteAck{version,events}`，实测集群 publish 响应含 changes |
| 🟠 deploy/Dockerfile 缺 protoc | ✅ builder 补 protobuf-compiler；compose 密码/令牌环境变量化 |
| 🟠 Go SDK ctx 失效 / HTTP watch 5s 掐断 | ✅ ctx 贯通（Watch 可取消）、SSE 独立无超时 client |
| 🟠 Admin UI 草稿编辑器数据破坏 | ✅ 按类型渲染回读、NaN 显式报错、分支保持、401 跳登录 |
| 🟠 KEK 明文入 Raft 日志（F7） | ✅ kek_enc 自加密（实测日志 `"kek":[]`）+ redb 0600 |
| 🟠 F4/F5/F8/F9/D-PRUNED/D-TYPE/F14/D-DEL | ✅ 全部落地（详见 §4.1/§7） |

**残留风险（低危/设计取舍，详见 §4.4/§3.3）**：HTTP 数据面鉴权已实现（D2，`--data-plane-token`
可选配置，未配置仍开放——演示兼容设计）；Admin CSP 已移除 unsafe-inline（D-CSP）；`/metrics` 无鉴权
（主密钥存在性 oracle，低）；契约测试断言已强化（D-TEST，ListMembers 真断言/断线续传自动化）。
剩余均为 P4 产品化项（灰度/RBAC/生态/TLS 内置）与低危运维提示。

**亮点（同类对比下的优势）**：单二进制交付（8.68MB）、AES-256-GCM 信封加密 + KEK 环轮换（含自加密
日志载荷）、Raft 集群 + 混沌容错、三语言 SDK 契约对拍、secret 全链路脱敏策略（现已无泄漏面）、
版本不可变 + 回滚 + 共享库级联的完整发布模型。详见 §6 竞品对比。

---

## 1. 项目概况与规模

| 维度 | 数值 |
|------|------|
| 仓库跟踪文件 | ~131（不含缓存/产物；`.gitignore` 覆盖完整，含 `.cargo-local/`） |
| Rust 代码 | ~14,900 行 / 12 crates（api 4500+/core 5400+/raft 2200+/cli 700+/crypto 440+/storage 390+…；v2 新增 dsh-core/wire.rs） |
| SDK | TS + Go + Python 共 ~3,230 行（v2 修复 ctx/退避/去重） |
| Admin UI | 单文件 `dsh-api/admin/index.html`（rust-embed 内嵌；v2 修复草稿编辑器） |
| 测试 | **130 个用例全绿**（本机实测，v2 新增 7 个）+ 3,635 行集成测试 + 6 个 e2e 脚本 |
| 契约 | openapi 37 paths / proto 4 RPC / storage schema 16+ $defs |
| 基准（文档实测） | 读 35k QPS（本机）/ 写 1.6k QPS / watch 12ms / 内存 41MB / 二进制 8.68MB |
| 提交历史 | main 主线，"审查→修复→再审查"闭环（M0–M8 + P0–P3 + 安全修复 4 轮） |

---

## 2. 代码质量

### 2.1 分层架构 —— ✅ 优

```
dsh-cli       组装器（646 行，仅装配，无业务逻辑）
dsh-api       HTTP/gRPC 面 + 鉴权 + Admin UI 内嵌（3207 行，偏重）
dsh-publish   发布编排（提交前加密/发布/回滚/结构发布）
dsh-raft      openraft 集成（store/network/http server）+ 通用写路径
dsh-core      确定性状态机 + 模型 + 校验 + 限额（1889 行状态机）
dsh-storage   redb 4 表单库（Store trait）
dsh-crypto    AES-256-GCM 信封加密 + KEK 环
dsh-{render,watch,jobs,observability,testkit}  单职责小模块
```

写路径统一收敛到 `dsh_raft::write_command`（`raft.rs:161-215`），dev-single 直 apply、集群走
`client_write` + leader 转发提示；加密在 apply 之外执行（保证 Raft 重放确定性，I8 纪律执行到位）。
crate 间依赖方向正确（core ← 各层，无环）。

### 2.2 确定性状态机 —— ✅ 优（全项目最扎实的部分）

- apply 不读墙钟/不 IO/不日志（D16），时间戳由命令载荷 `ts` 字段携带（`state.rs:423-429`，C2 修复）；
- 会话判定只判 `is_some()` 不读墙钟（`state.rs:1522,1636`），保证跨节点重放一致；
- 加密密文进状态机、明文只在 API 输入侧（`publish.rs:70-105`）；
- 幂等键（`last_request_id`，I10）、版本不可变、回滚=新版本（I6/I9）、共享发布原子级联（D15）全部落地且有测试；
- 密钥轮换（RotateMasterKey）经 Raft 复制 + 钩子幂等（重放安全），状态机本身零落库副作用（`state.rs:1725-1727`）——设计非常干净。

### 2.3 错误处理 —— 🟡 良（有统一体系，有 panic 面）

统一 `ErrorKind`（16 码）+ `ApiError` 状态码映射 + gRPC 独立 `map_err`，错误码字符串稳定
（`error.rs` 有稳定性测试）。弱点：
- **~20 处 `expect("sm lock")`**（`lib.rs` 329/509/527/583/785/921/991/1227/1356/1401/1538/1639/1665/1774/1876/2040/2113/2331/2757/2790）——Mutex 中毒即请求级 panic；
- `LeaderRedirect` 映射为 409（`lib.rs:172`），与真实冲突不可区分（F11）；
- 读路径多处 `if let Ok` 静默吞错（HTTP watch 重放 `lib.rs:2841-2866`；AuditLog 尽力而为系文档化设计）。

### 2.4 并发与锁 —— 🟡 良（简单正确，牺牲并发）

全局单把 `std::sync::Mutex<StateMachine>` 串行化读写。正确性优先（无跨 await 持锁、锁作用域化），
但读多写少场景无并发读；`StateMachineStore::apply` 持锁期间做事件广播 + rotation 钩子文件 I/O
（`store.rs:543-571`，锁内 IO 隐患，规模相关）。watch 用 1024 容量 broadcast。

### 2.5 复杂度与重复 —— 🟡 中

- `dsh-api/src/lib.rs` 单文件 **3200+ 行**过重（路由/鉴权/登录/PA/轮换/指标/watch 全在一处）；
- login / pa_login / rotate_master_key 的"LeaderRedirect→补 http://→转发→透传"样板 **v2 已收敛为
  `forward_request` 统一 helper**（含超时与 XFF 透传）；
- **掩码实现 v2 已收敛**为 `dsh-core/wire.rs` 单一出口（`masked_value`/`mask_event_for_wire`），
  消除初版"四套并存漏掩码"的温床（F1/F2 根因）；
- `dsh-core/src/state.rs` 1889 行、单文件 5000+ 行的核心模块建议后续拆分。

### 2.6 注释与文档 —— ✅ 优

中文注释质量高，设计决策（D1–D16、I1–I10、N/S/C 修复编号、F1–F20 修复编号）全程可追溯；
`docs/` 下 proposal、feasibility、design-v1/v2/v3、15 份模块规格、progress、remaining-work、
code-review、deep-analysis、fix-design 构成完整证据链。**业务代码零 `unsafe`**。
唯一缺憾：progress.md 中"76 tests"已落后于实测 **130**（历史文档，O4 类，不影响当前状态）。

### 2.7 测试质量 —— ✅ 优

**130 用例**覆盖：状态机全命令路径（含幂等/限额/级联/轮换）、加密 roundtrip 与轮换可解性、
**F7b 自加密 wrap/unwrap 与错误 KEK 拒绝**、**wire 掩码（事件无密文/原事件不受影响）**、
redb 崩溃重开/备份/多表隔离/**db 文件 0600 断言**、Raft 集群 3 节点 kill/重启追赶（真实多进程）、
gRPC 集成（真实 TCP）、PA 授权矩阵（653 行）、XSS/键名注入负例、登录节流窗口、
**可信代理 CIDR 匹配与节流键（XFF 仅在可信代理内生效）**、ring 文件 0600 断言（unix）。
**缺口**：network partition（RAFT-002）、慢消费者自动化（WCH-002，语义已实现仅缺脚本）、
SDK 幂等重试契约（SDK-002）仍未自动化（D4 已知）。

### 2.8 代码质量小结（v2 修复后）

| 子项 | 评分 | 一句话 |
|------|------|--------|
| 架构分层 | 9/10 | 边界清晰，依赖方向正确（wire.rs 归 core 合理） |
| 确定性设计 | 9.5/10 | D16 纪律执行彻底（WriteAck 事件随 apply 返回仍保持确定性） |
| 错误处理 | 7/10 | 体系统一；转发样板收敛为 forward_request（含超时/XFF）；expect("sm lock") 仍 ~20 处 |
| 并发 | 6/10 | 简单正确，无并发读、锁内 IO |
| 复杂度 | 6.5/10 | 掩码收敛为 wire.rs 单一出口、转发三处样板合一；lib.rs 仍 3300+ 行 |
| 注释/文档 | 9.5/10 | 证据链完整（含本轮修复编号 F1–F20 全程可追溯） |
| 测试 | 9/10 | 130 用例 + 4 个 e2e 实测全过；具名用例未全覆盖 |

---

## 3. 完成度

### 3.1 里程碑对照（docs/progress.md 声明 vs 实测）

| 里程碑 | 声明 | 实测证据 |
|--------|------|----------|
| M0 契约+脚手架 | ✅ | proto/openapi/schema 三件套存在，check-contracts 有脚本 |
| M1 单节点+集群 | ✅ | openraft 集成、--dev-single/--bootstrap/--join 在 cli 全实现 |
| M2 发布引擎+加密+渲染+任务 | ✅ | rollback/共享级联/引用物化/AEAD/三格式渲染/裁剪任务均在 code 中验证 |
| M2.5 watch+可观测+会话+UI | ✅ | SSE/metrics/readyz/审计/单会话/内嵌 /admin 均在 |
| M3 三语言 SDK | ✅ | TS/Go/Python 客户端 + 契约测试脚本在 |
| M4 混沌+加固+发布 | ✅ | chaos-test.sh、安全头、Docker/compose、README 在 |
| M5 会话落Raft+审计+快照持久化 | ✅ | sess/admin 入状态机、audit/{seq}、快照落盘（redb snapshots 表） |
| M6 模块化+gRPC | ✅ | 4 crate 归位、tonic ConfigService 4 RPC |
| M7 组级引用+密钥轮换 | ✅ | RefBinding item_key=None、KeyRing 轮换+Raft 复制 |
| M8 CI+基准+SBOM | ✅ | ci.yml 8 jobs、bench.sh、SBOM 工作流 |
| P0–P3 补全 | ✅ | 11 端点补全/掩码策略/gRPC SDK/Admin UI 控制台/CLI admin/指标 |

**结论：完成度声明与代码高度一致，未见"纸面完成、代码缺失"的造假迹象**（抽查了各里程碑的代表性代码路径均真实存在且可运行）。这是本报告给完成度打高分的最重要依据。

### 3.2 契约完整性

- **openapi 37 paths**：除 `/api/v1/projects/{p}/admins`（GET/POST）、`/admins/{u}`（DELETE/PUT）、
  `/admin/{*path}` 3 条**未登记**（F10，PA 账号管理面属安全敏感端点，缺文档），其余对齐；
- **proto 4 RPC**：GetConfig/GetItem/Watch/ListMembers 与实现一致，但**服务端 GetItem RPC 实际是死代码**
  ——三语言 SDK 全部用"拉全量+本地查找"实现（sdk/go/configclient/grpc_client.go:105-116 等）；
- **schema**：storage.v1.schema.json $defs 覆盖全部实体，模型 serde 对齐有测试（model_serde.rs）。

### 3.3 已知设计偏差（remaining-work.md D1–D5，均文档化接受）

| # | 偏差 | 影响评估 |
|---|------|----------|
| D1 | CLI 旋钮（--read-mode/--publish-policy/--shared-cascade 等）未实现 | 低：当前行为即设计默认 |
| D2 | **HTTP 数据面（/v1 snapshot/watch）无 token 鉴权** | 中→低（v2）：F1 修复后 secret 密文已不出网，非 secret 值依赖 TLS+网络隔离；可加 --data-plane-token（P3） |
| D3 | SDK 未实现 leader redirect 跟随 | 低：数据面读请求任意节点本地可服务 |
| D4 | RAFT-002/WCH-002/SDK-002 未自动化 | 低：手工/半自动覆盖 |
| D5 | device_id 绑定未实现 | 低：单会话机制已收敛并发；节流/argon2 已补 |

### 3.4 完成度缺口（v2 修复后复核）

以下 5 项初版缺口在 v2 **全部修复并实测**：
1. ~~集群模式写响应数据缺失（F6）~~ ✅ `WriteAck{version, events}`；实测集群 publish 响应含
   `changes`、结构发布 `affected_branches` 非空、共享发布 `affected` 非空，dev-single 与集群一致；
2. ~~HTTP/gRPC watch 重放事件类型失真~~ ✅ `VersionRecord.event_ty` 落标（structure_publish/
   shared_cascade/rollback 保真），旧日志回退推断；
3. ~~SSE 慢消费者静默丢事件（F5）~~ ✅ `take_while(is_ok)`：Lagged/关闭即结束流（不再静默丢）；
4. ~~版本裁剪后断线续传静默丢事件~~ ✅ gRPC 与 HTTP 重放前检测起点被裁剪 →
   发 `snapshot_required` 事件并关流（SSE 合成事件带该标志）；
5. ~~deploy/Dockerfile 缺 protoc~~ ✅ builder 补 `protobuf-compiler`；compose 密码/令牌环境变量化。

### 3.5 完成度小结

**9.5/10。** M0–M8+P0–P3 全部达成且可验证，v2 修复后集群/单机行为一致、部署链路可构建、
watch 可靠性（慢消费者/裁剪续传/类型保真）闭环；契约/文档/实现三方对齐度在同类项目中罕见。
剩余扣分：HTTP 数据面鉴权（D2，设计取舍）、PA 账号路由未登记 openapi（F10，文档项）、
契约测试断言强度（D-TEST）。

---

## 4. 安全性

### 4.1 上轮审查（code-review.md S/C/R）修复状态 —— 本报告逐一代码复核

| 项 | 结论 | 代码证据 |
|----|------|----------|
| S1 Admin UI XSS | ✅ 已修 | `validator.rs:71-77` valid_key_name 字符集封死 + 前端 esc() 全量补齐 |
| S2 join 无鉴权 | ✅ **v2 已强制** | 集群模式启动强制 `--join-token`（缺失报错退出）；脚本/compose/README 全部注入 |
| S3 主密钥轮换集群一致 | ✅ 已修 + **v2 F7b 自加密** | RotateMasterKey 经 Raft + 幂等钩子；`kek_enc` 自加密载荷（日志无明文）；dev-single 先持久化后切换 |
| S4 ring 文件 0644 | ✅ 已修 | `save_ring` 0o600 + set_permissions（`crypto/lib.rs`），含 unix 权限断言测试 |
| S5 raft RPC 无鉴权 | ✅ **v2 已强制** | 集群模式强制 `--raft-token`（与 join-token 同批落地） |
| S6 登录限次+哈希 | ✅ 已修 + **v2 F4 加固** | LoginThrottle + argon2 PHC；节流键改对端 IP（`--trusted-proxy` 才信 XFF，不可伪造） |
| C1 operator="test" | ✅ 已修 | dsh-publish 四方法透传（`publish.rs:124,150,192,223`） |
| C2 集群时间戳=日志序号 | ✅ 已修 | 8 命令变体 `ts` 字段 + `eff_ts` 统一解析（`state.rs:423-429`） |
| C3 共享键注入 | ✅ 已修 | 共享 group/key 与引用绑定接入 valid_key_name |
| R1 raft RPC 无超时 | ✅ **v2 已补全** | raft 客户端 connect 3s+total 60s；API 转发统一 `forward_request`（connect 3s + total 10s） |

### 4.2 v2 修复状态总览（初版 F1–F20 → 实施结果）

> 初版 §4.3–4.4 的问题描述保留在下方作为历史证据；以下为本轮实施状态
> （详见 [code-review.md](code-review.md) §8 第 4 轮，全部经 `cargo test` 130 用例 + e2e 实测）。

| 项 | 状态 | 实测/要点 |
|----|------|-----------|
| F1 HTTP watch 密文泄漏 | ✅ 已修复 | `dsh-core/wire.rs` 统一掩码，watch 事件输出 `{"type":"string","str_value":"***"}` |
| F2 branch_diff 密文泄漏 | ✅ 已修复 | 同一掩码函数输出 |
| F3 join 默认开放 | ✅ 已修复 | 集群模式强制 token（见 4.1 S2） |
| F4 登录节流键可伪造 | ✅ 已修复 | `--trusted-proxy` CIDR + 对端 IP 键；转发透传 XFF；单测 3 个 |
| F5 SSE 慢消费者丢事件 | ✅ 已修复 | `take_while(is_ok)` 结束流（不再静默丢） |
| F6 集群写响应 events 恒空 | ✅ 已修复 | `WriteAck{version,events}`；集群 publish 响应实测含 changes |
| F7 KEK 明文入日志 + redb 0644 | ✅ 已修复 | `kek_enc` 自加密（日志 `"kek":[]`）；redb open 后 chmod 0600 + 断言测试 |
| F8 转发客户端无超时 | ✅ 已修复 | `forward_request`（connect 3s + total 10s）三处统一 |
| F9 secret 共享项明文 | ✅ 已修复 | API+状态机双层校验（secret 项必须 type=secret 且字符串） |
| F10 openapi 缺 PA 路由 | ✅ 已修复（P3） | 补 /admins 4 条路径 + ProjectAdminAccount/PAUpsertRequest schema（openapi 39 paths） |
| F11 LeaderRedirect→409 | ✅ 已修复（P3） | 独立状态码 428（Precondition Required），body 仍带 ERR_LEADER_REDIRECT + leader_hint；实测非 leader 写返回 428 |
| F12 CSP unsafe-inline | ✅ 已修复（P3） | Admin 脚本外置 app.js + data-act 事件委托；`script-src 'self'`（实测 header 无 unsafe-inline、UI 全流程零 JS 错误） |
| F13 expect("sm lock")×20 | ✅ 已修复（P3） | 29 处全收敛：handler 用 `lock_err` helper（500）、watch 只读取内部值、observability 告警/into_inner |
| F14 join 无校验 | ✅ 已修复 | node_id 未占用 + http/raft 地址 host:port 校验 |
| F15 deletes 静默丢弃 | ✅ 已修复 | 无 `/` 条目返回 422 |
| F16 重放吞错 | ⏳ 残留（低） | `if let Ok` 模式仍在（C5 类，存储错误罕见） |
| F17 gRPC 重放 O(n) | ⏳ 残留（低） | 性能项，大历史场景优化未排期 |
| F18–F20 | ⏳ 残留（低） | last_applied 条件写 / 广播 send 忽略 / 硬编码 admin（均低危） |
| Dockerfile 缺 protoc | ✅ 已修复 | builder 补 protobuf-compiler；compose 环境变量化 |

### 4.3 初版高危记录（已修复，保留原文供追溯）

**F1 🔴 HTTP watch 对未鉴权客户端泄漏 secret 密文**（v2 已修复）
初版：`/v1/.../watch` 无鉴权，`watch_sse` 直序列化 `PublishEvent`，`Value::Secret` 完整输出
ciphertext（`model.rs:137-140`）。**修复**：`watch_sse` 出网前统一 `mask_event_for_wire`；实测无密文。

**F2 🔴 `branch_diff` 向项目管理员泄漏 secret 密文**（v2 已修复）
初版：`lib.rs:946-950` 原样输出 `Value::Secret(ct)`。**修复**：diff 值经 `masked_value` 输出。

### 4.4 初版中危记录（已修复，保留原文供追溯）

| # | 项 | v2 状态 |
|---|-----|---------|
| F3 | join 默认开放 | ✅ 强制 token |
| F4 | 登录节流键可伪造 | ✅ 对端 IP + 可信代理 |
| F5 | HTTP watch 慢消费者静默丢事件 | ✅ 流结束 |
| F6 | 集群写响应 events 恒空 | ✅ WriteAck |
| F7 | KEK 明文入 Raft 日志 + redb 权限 | ✅ kek_enc + 0600 |
| F8 | login/rotate 转发无超时 | ✅ forward_request |
| F9 | secret 共享项非 String 不加密 | ✅ 校验拒绝 |

### 4.5 初版低危记录（部分已修复，保留原文供追溯）

F14 ✅（join 校验）、F15 ✅（deletes 422）；F10/F11/F12/F13/F16/F17/F18/F19/F20 残留（低危/P3）。
另（本报告补充）：`/metrics` 无鉴权输出 `dsh_master_key_ok`（主密钥存在性 oracle，低）；
`--admin-password` 落命令行可见于进程列表（运维提示）；无密钥演示模式下 secret 项明文存储（文档化演示模式）。

### 4.6 安全小结（v2 修复后）

**8.5/10。** 加密体系（信封加密 + KEK 环 + 自加密日志载荷 + 0600 ring/db 文件）与认证体系
（argon2 + 单会话 + 审计 + 授权矩阵 + 强制 join/raft token + 对端 IP 节流）设计正确且闭环：
**数据面脱敏承诺已无泄漏面**（F1/F2/F9 全部封死）。残留均为低危/设计取舍：HTTP 数据面无 token
（D2，secret 已不出网，非 secret 依赖网络隔离）、CSP unsafe-inline（P3）、/metrics 信息泄露（低）、
~20 处 expect（低）。**后续建议：P3 清单（D-CSP/D-STATUS/F10/F13）+ D2 的 HTTP data-plane-token。**

---

## 5. 功能闭环

### 5.1 配置生命周期 —— ✅ 完整闭环

项目 → 分支（dev/test/prod+自定义）→ 结构（不可变版本）→ 值草稿 → 发布（幂等）→ 版本（不可变）→
回滚（新版本）→ watch 通知（SSE/gRPC）→ 多格式渲染（YAML/TOML/JSON）→ 版本历史/对比/promote。
共享库（草稿→发布→级联→引用绑定/解绑，item 级+组级）与 secret 加密存储贯穿。**闭环完整；**
v2 修复后集群/单机发布响应均携带 changes/affected（F6 实测）。

### 5.2 集群闭环 —— ✅ 完整

--bootstrap/--join（v2 起强制 join-token）/promote（learner→voter）/remove-node（F14 校验）、
leader 击杀容错、重启自动恢复（raft-meta 判据）、快照持久化与追赶、leader 转发提示
（ERR_LEADER_REDIRECT + hint）、集群 watch（apply 广播）、集群主密钥轮换（kek_enc 自加密经
Raft，v2 实测跨节点可解）。chaos-test.sh 实证 leader SIGKILL→选举→追赶一致。**闭环完整。**

### 5.3 watch 闭环 —— ✅ 完整（v2 修复后）

SSE 与 gRPC 双通道、after_version 断线续传（历史重放+实时去重）、慢消费者 → 结束流/关流
（F5，gRPC 发 snapshot_required、SSE 结束流）、**起点被裁剪 → snapshot_required**（D-PRUNED，
v2 双侧落地）、**重放事件类型保真**（D-TYPE，`event_ty` 落标）。**残留**：gRPC/HTTP 重放
O(n) 全量快照 diff（F17，大历史下昂贵，性能优化项）。

### 5.4 SDK 闭环 —— ✅ 工程缺陷已修复（契约对拍仍待强化）

三语言契约测试（HTTP + gRPC）对拍通过；after_version 续传三语言均有。v2 修复：
- Go gRPC **ctx 贯通**（Watch 可取消，不再泄漏 goroutine）；**SSE 独立无超时 client**（不再 5s 掐断）；
- TS **请求 10s 超时**、动态 import 失败回落 HTTP、声明修正（gRPC Node-only）、watch 版本去重；
- Python **gRPC watch 指数退避**、事件去重、空端点报错、tls no-op 标注；
- 三语言 watch 均按版本去重（重放/重连不重复投递）。

**残留**：三语言 `getItem` 仍拉全量（服务端 GetItem RPC 死代码）；HTTP 通道 version 参数静默丢弃；
契约测试断言仍弱（get 不校验值、ListMembers 零断言、无断线续传/慢消费者自动化，D-TEST）。

### 5.5 Admin UI 闭环 —— ✅ 数据编辑缺陷已修复（功能面仍缺入口）

登录/项目 CRUD/结构草稿+发布/分支草稿表单（含 secret）/发布/历史+回滚/对比+promote/共享库/
审计/SSE watch 全覆盖，XSS 转义总体全面。v2 修复：**草稿编辑器按类型渲染回读**（int/float/
json/array/bool 正确回显与保存，NaN 显式报错）、**secret 留空不改**（避免把 "***" 当明文提交）、
**分支选择保持**（不再跳回第一个）、**401 跳登录**。
**残留**：EventSource 不带 after_version（断线丢事件，低）；缺 reveal/集群管理入口（功能面）。

### 5.6 管理运维闭环 —— ✅ 完整

CLI `dsh admin` 8 子命令（gen/rotate-master-key、force-logout、set-password、promote、
remove-node、snapshot、retention-status）+ 对应 HTTP 端点 + 审计；/healthz /readyz /metrics
/audit API；后台任务（版本裁剪/审计保留/DEK 重包，仅 leader）；备份（redb 文件级）。
v2 新增 `--trusted-proxy` 运维旋钮。

### 5.7 功能闭环小结

**9/10。** 核心业务闭环（配置生命周期/集群/管理运维/watch）完整且经 e2e 实测；v2 修复后
"集群变更明细缺失、watch 可靠性、SDK 工程缺陷、UI 数据破坏"四大断点全部消除。
剩余扣分：契约测试断言强度（D-TEST）、UI 功能面缺口（reveal/集群管理入口）、GetItem 死代码。

---

## 6. 竞品对比

### 6.1 竞品图谱

| 系统 | 定位 | 共识/ | 集群模型 | 语言 | 许可 |
|------|------|--------|----------|------|------|
| Apollo（携程） | 配置中心 | 生产验证充分、热发布/灰度/多环境强 | 无内置强一致（Eureka 注册 + 自研通知，DB 为事实源） | Java | Apache-2.0 |
| Nacos（阿里） | 注册+配置 | 云原生生态、动态 DNS | Raft（配置/Distro 协议） | Java | Apache-2.0 |
| etcd | 分布式 KV | 通用强一致底座（K8s 依赖） | Raft | Go | Apache-2.0 |
| Consul | 服务发现+KV+配置 | 生态完整，多数据中心 | Raft | Go | BSL（2023 起非开源） |
| Spring Cloud Config | 配置服务 | Spring 生态、git 后端 | 无（依赖外部后端） | Java | Apache-2.0 |
| HashiCorp Vault | 密钥管理 | 动态密钥/策略/审计最强 | Raft（HA） | Go | BSL（2023 起非开源） |
| **Defing（本项目）** | 配置+文档 | 单二进制、确定性状态机、secret 信封加密 | openraft（Raft） | Rust | Apache-2.0 |

### 6.2 功能矩阵

| 能力 | Apollo | Nacos | etcd | Consul | Vault | **Defing** |
|------|:------:|:-----:|:----:|:------:|:-----:|:----------:|
| 强一致集群 | ⚠️（最终一致+DB） | ✅ | ✅ | ✅ | ✅ | ✅ openraft |
| 结构约束/类型校验 | ⚠️ 弱 | ⚠️ 弱 | ❌ | ❌ | ❌ | ✅ 结构=一等公民 |
| 草稿→发布→回滚闭环 | ✅ | ⚠️（版本有，草稿弱） | ❌ | ❌ | ⚠️ | ✅ 完整+幂等 |
| 多环境（项目/分支） | ✅ 多环境 | ✅ namespace | ❌ | ⚠️ KV | ❌ | ✅ 分支模型 |
| 共享配置/级联 | ✅ 公共配置 | ✅ 共享配置 | ❌ | ❌ | ❌ | ✅ item/组级引用+原子级联 |
| 热更新推送 | ✅ | ✅ | ✅ watch | ✅ | ✅ watch | ✅ SSE+gRPC 双通道+续传 |
| secret 加密 | ⚠️（v1.7+ 需配置） | ❌（明文为主） | ❌ | ⚠️ | ✅ 最强 | ✅ AES-256-GCM 信封+KEK 轮换 |
| 审计 | ⚠️ | ⚠️ | ⚠️ 弱 | ✅ | ✅ 最强 | ✅ 状态机落库+operator |
| RBAC/细粒度权限 | ✅ 完善 | ✅ | ⚠️ | ✅ ACL | ✅ 最强 | ⚠️ 单管理员+项目管理员（无 RBAC） |
| 多语言 SDK | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ TS/Go/Python（gRPC+HTTP） |
| 部署形态 | 重（Eureka+DB+Portal+Config） | 重（3 组件） | 轻 | 轻 | 重 | **单二进制**（含 UI） |
| 性能（读 QPS 量级） | 万级 | 万级 | 十万级（etcd v3） | 十万级 | 万级 | 3.5 万（本机实测） |
| 生产成熟度/社区 | 高 | 高 | 极高 | 高 | 高 | **低（原型/教学级）** |
| 许可风险 | 无 | 无 | 无 | **BSL** | **BSL** | 无（Apache-2.0） |

### 6.3 Defing 的差异化定位

1. **单二进制 + 内嵌 UI**：与 etcd/Consul 同属轻量部署，但自带管理控制台与三语言 SDK，开箱即用；
2. **配置模型一等公民**：结构（类型/必填/secret 标记）+ 分支（dev/test/prod）是其他 KV 型竞品
   （etcd/Consul）不具备的语义层，比 Apollo/Nacos 的"KV+文本"更结构化；
3. **确定性状态机 + Raft**：apply 无副作用的设计（D16）使其具备**可重放、可审计**的强一致特性，
   这在竞品中少见（多数是"存储强一致+业务最终一致"）；
4. **secret 信封加密内建**：对标 Vault 的加密理念但聚焦配置场景（非通用密钥管理），
   AES-256-GCM + KEK 环 + 轮换 + DEK 重包，实现质量超过 Apollo/Nacos 的密码学水平；
5. **Apache-2.0**：在 Consul/Vault 转 BSL 的背景下有合规吸引力。

### 6.4 主要差距（相对竞品）

| 差距 | 说明 | 追赶成本 |
|------|------|----------|
| RBAC/多租户 | 仅"全局管理员 + 项目管理员"两级；Apollo 有部门/应用/环境三维权限，Nacos 有 namespace+角色 | 中（PA 框架可扩展） |
| 灰度/发布策略 | Apollo 有灰度发布/回滚策略；Defing 仅全量发布 | 中（design 已留 D1 旋钮） |
| 生态集成 | 无 Spring Cloud/K8s ConfigMap/服务发现集成 | 高 |
| 生产运维 | 无官方 Helm chart、无多数据中心、无 TLS 内置（需前置代理）、无持久化队列审计 | 中 |
| 性能上限 | 写路径单写者串行（1.6k QPS），etcd v3 批量可达数万 | 中（架构已定型） |
| 成熟度证据 | 无真实生产案例/压测基准归档（bench 仅为冒烟） | 时间 |

### 6.5 竞品对比小结

**定位判断**：Defing 不是 Apollo/Nacos 的替代品（生态与 RBAC 差距大），也不是 etcd/Consul 的
通用 KV 底座（无服务发现/lease），而是一个**"结构化配置 + 强一致 + 内建加密 + 轻量单二进制"**
的差异化产品。它在"配置语义层 + 加密 + 部署轻量"三角上有真实卖点。
**v2 修复后**：安全/可靠性基线（数据面脱敏、join/raft 鉴权、主密钥自加密、watch 不丢事件、
SDK 可用性）已对齐生产级要求，与竞品的差距进一步收敛为**产品面**——RBAC/灰度/生态集成/
TLS 内置/成熟度证据（P4 清单），进入企业选型池前主要补齐这些。

---

## 7. 修复优先级路线图（v2 后更新）

**✅ 已实施（P0 全部 + P1 全部 + P2 大部分 + P3 全部，共 27 项）**：
- P0：F1/F2/F3/F6/F7a/F7b、Dockerfile protoc；
- P1：F4/F8/F9、SDK 三语言、Admin UI 数据修复；
- P2：F5/D-PRUNED/D-TYPE/D-JOIN/D-DEL/D-DOC（契约文档）；
- **P3（本轮全部完成）**：D-CSP（Admin 脚本外置 app.js + data-act 委托 + CSP 移除 unsafe-inline）、
  D2（HTTP 数据面 --data-plane-token，header + ?token=）、D-STATUS（LeaderRedirect → 428）、
  F10（openapi 补 PA 路由 4 条）、F13（29 处 expect("sm lock") 全收敛）、D-TEST（ListMembers
  三语言真断言 + after_version 重放 + 事件字段断言 + 慢消费者/snapshot_required Rust 测试）、
  UI 功能面（reveal 入口 / 集群管理 tab / EventSource after_version + 事件截断）、O2（compose 非 root）。

**回归验证（本轮实测）**：132 测试全绿（+2 D-TEST）、clippy/fmt 零告警、4 个 e2e 全过、
SDK gRPC/HTTP 契约测试全过（含新断言）、D2 鉴权 401/200 实测、D-STATUS 非 leader 428 实测、
Admin UI 浏览器全流程（登录/建项目/集群 tab/零 JS 错误）。

**剩余（P4 产品化，非缺陷）**：

| 优先级 | 项 | 说明 |
|--------|-----|------|
| P4 | 灰度发布 | Apollo 有灰度/回滚策略；Defing 仅全量发布 |
| P4 | RBAC 扩展 | 分支级权限、多租户 namespace |
| P4 | 生态集成 | Spring Cloud / K8s ConfigMap / 服务发现 |
| P4 | 运维体系 | Helm chart、多数据中心、TLS 内置 |
| P4 | 成熟度证据 | 生产案例、压测基准归档 |

---

## 8. 附录：方法与证据

- **实测（v2）**：`cargo test --workspace` 130 passed / 0 failed；`cargo clippy --all-targets` 零告警；
  `cargo fmt --check` 干净；dev-single-demo、api-surface-test（13 组断言）、cluster-demo、chaos-test
  四个 e2e 脚本全过；运行时实测——HTTP watch 无密文（F1）、无 token 启动被拒（F3）、集群 publish
  响应含 changes（F6）、集群轮换（共享主密钥）跨节点旧数据可解 + 日志 `"kek":[]`（F7b）、
  Python SDK get/watch/去重（SDK）。
- **人工复核**：state.rs、model/command/validator/keys/error/limits/diff、crypto、render、watch、
  jobs、observability、storage、publish、testkit、cli main、grpc.rs、proto/openapi/schema、
  build.rs、CI、Docker/compose、design-v3、project-admin 设计；v2 修复全部经代码级复核。
- **子代理复核**：dsh-api/src/lib.rs 全文 + dsh-raft 全量；三语言 SDK 全量 + Admin UI 单文件 +
  契约测试脚本（发现 F1–F20，v2 已实施）。
- **参考**：HashiCorp BSL 变更 [globenewswire](https://www.globenewswire.com/news-release/2023/08/10/2723189/0/en/hashicorp-adopts-the-business-source-license-for-future-releases-of-its-products.html)、
  [TechTarget](https://www.techtarget.com/searchitoperations/news/366548272/Confusion-mounts-amid-HashiCorp-open-source-change)；
  配置中心选型对比 [datasea](https://datasea.cn/go07251543.html)、[Apollo 概述](https://cloud.tencent.cn/document/product/1364/58363)。
