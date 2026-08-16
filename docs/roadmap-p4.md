# Defing P4 升级路线图 —— 灰度 / RBAC / 生态集成

> 版本：v1.0 ｜ 日期：2025-08-16
> 依据：[deep-analysis-2025.md](deep-analysis-2025.md)（v2，F1–F20 修复后复核）P4 产品化清单、
> [remaining-work.md](remaining-work.md)（D1–D5）、[project-admin.md](design/project-admin.md)（PA 设计 v3）。
> 本路线图逐项经**代码级即时复核**（下述"现状（代码证据）"小节全部来自 main `bf030d5` 实测定位），
> 非仅引用报告结论。

---

## 0. 执行摘要

三块 P4 升级的**目标与前置关系**：

```
D1 发布策略旋钮（--publish-policy / --shared-cascade）──► 灰度发布（G 线）
Action-based 授权重构（pa_allowed → authorize(action)）──► RBAC 分支级权限（R 线）
TLS 内置 + 契约硬化（D-TEST 收尾）────────────────────► 生态集成（E 线）
```

- **总工期预估**：单人力 10–14 周（G 线 5–6 周、R 线 4–5 周、E 线 6–8 周，三线共享地基可并行）。
- **红线不变**：状态机确定性（D16，灰度规则存状态、解析在数据面读时；权限存状态、判定在中间件）、
  Raft wire 兼容（新命令纯新增、旧变体不动、新字段 `#[serde(default)]`）、默认拒绝无绕过面（N2 纪律）。
- **建议启动顺序**：先并行做三块各自的地基（G1 / R1 / E0+E1），再做各自主功能；审批流（R4/G 联动）最后。

---

## 1. 灰度发布（P4-A）

### 1.1 现状（代码证据）

| 维度 | 现状 | 代码定位 |
|------|------|----------|
| 发布粒度 | 分支整体发布，无放量/定向概念 | `Command::Publish{project,branch,…}`（command.rs:83）；`apply_publish` 全量固化草稿→版本→指针（state.rs:946-1063） |
| 活动版本 | 每分支单指针 `active_version`，无灰度共存版本 | `BranchState{active_version,…}`（model.rs:234-242）；`get_config` version=0 → `active_version`（state.rs:398-402） |
| 数据面身份 | 客户端无身份上下文，读请求不带实例/IP/标签 | proto `GetConfigRequest{project,branch,version}`（config.v1.proto:63-67）；HTTP `/v1/.../snapshot`（lib.rs:1732） |
| 发布策略旋钮 | **D1 全部未实现** | CLI 仅 18 个 flag，无 `--read-mode/--publish-policy/--shared-cascade/--watch-event-retain`（main.rs:38-131） |
| 回滚 | 回滚=新版本（I6/I9），无"灰度回滚/一键下量" | `apply_rollback`（state.rs:1068） |
| watch | 按 (project,branch) 订阅，事件带版本 | proto `WatchRequest`（config.v1.proto:95-99）；SSE/gRPC 双通道 |

**结论**：与报告 §6.4 一致——"仅全量发布，追赶成本中，design 已留 D1 旋钮"。代码层面灰度需**新增状态字段 + 新命令 + 数据面身份解析**三件事，均不破坏现有发布闭环。

### 1.2 目标模型（参考 Apollo 灰度 + Defing 确定性约束）

```
分支（prod）下双版本共存：
  active_version（全量稳定版）  +  gray_version（灰度版，可选）
  gray_rule（灰度规则，存状态机，确定性）：
    按实例标签  {match_labels: {"zone":"cn-north-1","svc":"checkout"}}
    按 IP 段    {ip_cidrs: ["10.0.0.0/24"]}
    按百分比    {percentage: 10}            // 对客户端身份哈希分桶
数据面解析（读时，不进 apply）：
  get_config(project, branch, client_ctx) →
     命中 gray_rule 且 gray_version 存在 ? gray_version : active_version
watch：
  事件携带 gray/stable 标记；SDK 按自身解析到的版本过滤，避免错收
灰度生命周期：gray publish（定向放量）→ 观察 → 全量发布（gray 提升为 stable）/ 灰度回滚（摘除 gray 指针）
```

关键纪律：**规则与版本指针都是状态机数据（确定性）；客户端身份只出现在请求侧（API 层），selector 求值在数据面读路径**，apply 不读墙钟/不读请求，D16 不破。

### 1.3 阶段计划

| 阶段 | 内容 | 关键代码面 | 验收 | 估时 |
|------|------|-----------|------|------|
| **G0 设计先行** | ✅ **已完成**（`docs/design/gray-release.md`，330 行含流程图 + 审核 Q1-Q6 修订记录）：模型选型、分支内双版本、身份传递、watch 语义、回滚语义；决策 D17–D23 落地 | — | 设计评审通过（子代理 Q1-Q6 全闭环，含 3 阻塞级修订） | ✅ |
| **G1 发布策略地基（D1 收尾）** | `--publish-policy=block\|warn`（warn 时发布校验失败仅记录 detail 继续）、`--shared-cascade=auto\|manual`（manual：共享发布只更共享版本，引用分支下次发布物化）、`--read-mode=linear\|stale` | cli main.rs 加参 → PublishService/apply 注入 policy；`apply_shared_publish` 拆 manual 路径（state.rs:1216） | 三旋钮 e2e 实测；对应 D1 偏差关闭 | 3–4 天 |
| **G2 灰度核心状态机** | ✅ **已完成**（`docs/plan-gray-g2.md` 13 任务全闭环）：新命令 `GrayPublish{project,branch,rule,comment,request_id,operator,ts}`、`GrayAbort{…}`、`GrayPromote{…}`（纯新增，旧变体不动）；`BranchState` 加 `gray_seq`/`gray_rule`（serde default）；灰度快照存独立前缀 `gray-snap/`（**独立灰度序号，不与 active_version 冲突**，Q1 修订；promote 用 `next=max(active,gray)+1` 单调分配器；结构发布灰度活跃时一次分配两个不同号，D23）；**复用既有 EventType + `gray:bool` 字段**（不新增枚举值，Q3 修订；PublishEvent/VersionRecord 均加，watch 重放保真）；补 I10 幂等；`resolve_version`/`rule_matches`/`fnv1a_hash` 读路径纯函数（**固定求值次序 labels→IP→percent；无身份永不进灰度**，Q2）；`prune_versions` 依赖 gray-snap/ 前缀隔离天然保留灰度快照（Q5，T8 实测）；`rewrap_deks` 覆盖灰度快照（轮换安全） | command.rs / model.rs / state.rs / keys.rs；core 测试先行 | `cargo test -p dsh-core` 全绿（新增 T1-T8：灰度发布/解析三路/转正/下量/幂等/错误路径/结构发布×gray/无身份不进灰度/保留策略，47 用例） | ✅ |
| **G3 数据面解析 + watch** | ✅ **已完成**（`docs/design/g3-dataplane.md`，D24-D28 + 审核 B1/R1-R3 全闭环）：读路径纯函数 G2 已备；本阶段落地——`resolve_version` 升级返回 `ResolvedVersion`（D24，消除 gray_seq/active 数值巧合歧义）；`get_config_resolved` 数据面统一入口（version=0 按身份 resolve，灰度命中读 gray-snap/；**R1：version=active（v/ 空间）、resolved_version=gray_seq**）；HTTP snapshot 身份头 X-Dsh-Instance/X-Dsh-Labels + PeerAddr IP；gRPC get_config/get_item instance_id/labels + remote_addr IP（Q6 get_item 同分流）；响应加 `gray`/`resolved_version`（D27）；watch 方案 b：`e.gray \|\| e.version > last`（SSE+gRPC，last 只增不减；**B1：重连必做 snapshot 拉取的 SDK 契约**）；proto 加字段（向后兼容）；**最小管理面 4 端点**（gray-publish/promote/abort/status，UI tab 留 G4）；`scripts/gray-demo.sh` 三路 e2e 全绿 | state.rs、lib.rs、grpc.rs、dsh-publish、dsh-watch、proto、gray-demo.sh | 数据面三路解析实测 ✅（demo 华北 gray-host/gray:true、华南/无身份 stable）；watch 灰度事件隔离实测 ✅（promote 补发 gray=true 投递）；workspace 31 套件全绿 + clippy 0 警告 | ✅ |
| **G4 灰度管理面 + UI** | HTTP：`POST /projects/{p}/branches/{b}/gray-publish`、`…/gray-abort`、`…/gray-promote`、`GET …/gray-status`；openapi 补路径；Admin UI 灰度 tab（规则编辑/状态/一键回滚）；审计 action 覆盖 | lib.rs handler、openapi.v1.yaml、admin/index.html+app.js | api-surface 新增断言组全过；浏览器全流程实测 | 4–5 天 |
| **G5 百分比放量 + 观察** | 身份哈希分桶（确定性，文档化算法）；灰度期间 metrics（`dsh_gray_active` 等）；"一键回滚"= GrayAbort；自动回滚钩子（对接 /metrics 指标，可选） | observability、jobs（自动回滚任务，仅 leader） | 百分比放量跨节点一致（Raft 重放同一规则同一桶）；自动回滚触发实测 | 4–5 天 |

**SDK 三语言适配**（G3/G4 同步，每语言 1–2 天）：`ConfigClient` 增加 `instance/labels` 选项、watch 事件过滤（gray:true 永不按版本过滤、**重连必做一次 snapshot 拉取**——B1 契约）、缓存版本号只取 snapshot 响应（R1：version=active、resolved_version=gray_seq）。服务端数据面已就绪（G3 ✅），三语言适配是让灰度端到端可用的最后一环。

### 1.4 风险

1. **watch 语义复杂度**：灰度共存时同一分支双版本事件流，SDK 端按解析版本过滤可能漏掉"从灰到全量"的切换事件——设计上要求 **GrayPromote 时向灰度客户端补发全量事件**（G0 决策点）。
2. **百分比哈希稳定性**：客户端身份字段变化（容器重建 IP 变）会导致分桶漂移——G0 需定"身份稳定键"优先级（hostname > stable instance id > IP）。
3. **结构发布 × 灰度**：结构发布当前推进所有分支版本；gray 版本需同步 bump 或标记失效（G2 测试必覆盖）。

---

## 2. RBAC 扩展（P4-B）

### 2.1 现状（代码证据）

| 维度 | 现状 | 代码定位 |
|------|------|----------|
| 主体 | 两级：全局管理员 + 项目管理员；**PA 一个账号仅绑定一个项目** | `Principal::Admin \| ProjectAdmin{username,project}`（model.rs:349-360）；`ProjectAdminAccount{username,project,…}`（model.rs:364-376） |
| 授权 | 路径字符串矩阵，默认拒绝、显式放行 | `pa_allowed()`（lib.rs:415-458）；token 前缀路由 `pa.{username}.{secret}`（lib.rs:1933） |
| 会话 | 每主体单会话，存状态机 | `sess/admin`、`sess/pa/{username}`；`PaSessionLogin` 等（command.rs:170-183） |
| 明确不做 | 账号多项目、argon2、密码复杂度、watch SSE 鉴权、Web 控制台 | project-admin.md §11 |

**结论**：与报告 §6.4 一致——"仅全局+项目两级，PA 框架可扩展，追赶成本中"。扩展的关键是**把 `pa_allowed` 的路径矩阵重构为 action 检查**，这是分支级权限与多项目的前置。

### 2.2 目标模型

```
主体：Admin（全局）｜ User{username}（多项目成员）＋ Membership{username, project, role}
角色（每项目）：viewer（只读）/ editor（草稿+发布）/ operator（+回滚/promote/灰度）/ admin（全项目面）
权限检查：authorize(principal, action, resource=(project, branch?))
  action 枚举：project.read / project.write / draft.edit / publish / rollback / gray.manage / shared.write / cluster.admin …
审批流（可选）：publish 到 prod 需 approver 角色二次确认（与灰度 G 线联动）
```

### 2.3 阶段计划

| 阶段 | 内容 | 关键代码面 | 验收 | 估时 |
|------|------|-----------|------|------|
| **R0 设计先行** | RBAC 模型文档（角色集、权限粒度=项目级/分支级/item 级、审批流归属、多租户边界）；决策：是否本期做多项目（建议：本期做角色+分支级，多项目做二期） | docs/（新增 `design/rbac.md`） | 设计评审通过；决策记录落档 | 2 天 |
| **R1 角色模型（状态机）** | `Role` 枚举 + `ProjectAdminAccount.role`（serde default 兼容）；新命令 `AccountRoleSet{username,role}`；分支级权限表 `Permission{principal,project,branch,actions}` + 命令 `BranchPermissionSet`；访问器 | model.rs / command.rs / state.rs / keys.rs | core 测试 ≥10 用例（角色变更审计/权限表读写/旧日志 default） | 4–5 天 |
| **R2 授权重构（action-based）** | `pa_allowed` 路径矩阵 → `authorize(principal, action, resource)`；handler 层声明所需 action；**保持默认拒绝 + N2 路径提取纪律**（`project_segment` 不动）；PA 读接口按分支权限收窄 | lib.rs auth_middleware / pa_allowed / 各 handler | 既有 PA 授权矩阵测试全量回归（http_project_admin.rs）+ 新增 action 级断言；无绕过面（%70/大写/尾斜杠组）保持 403 | 4–5 天 |
| **R3 多项目成员（二期或并入）** | `ProjectAdminAccount` 拆为账号 + `Membership{username,project,role}`；Principal 携带 username（project 改由成员表解析，**不信任 token**）；登录/会话/审计适配 | model.rs / state.rs / lib.rs token 解析 | 一账号多项目登录/切换/审计归属实测；既有单项目 PA 全量回归 | 5–7 天 |
| **R4 审批流（与 G5 联动）** | 发布策略 `policy=approval`；`PublishRequest`/`PublishApproval`/`PublishReject` 命令；approver 角色；审批挂起态不得阻塞其它分支 | command.rs / state.rs / api / UI 审批队列 | 审批闭环 e2e；审计含审批链 | 5–7 天 |
| **R5 多租户 namespace（远期）** | 项目分组为命名空间、独立管理域、配额隔离 | 全新实体，另立设计 | — | ≥2 周（远期） |

### 2.4 风险

1. **授权重构回归面大**：`pa_allowed` 是全项目唯一强制点（project-admin.md §10 风险 1），重构必须逐 action 对拍既有矩阵测试（http_project_admin.rs 653 行是现成护栏）。
2. **分支级权限 × 灰度**：谁能发布灰度/谁能提升全量需要独立 action，避免"editor 可灰度、不可全量"的割裂权限语义（R1 设计时把 gray.* 独立成 action）。
3. **多项目 Principal 形状变更**：`Principal::ProjectAdmin{username,project}` 改形会触及 Raft 会话数据（旧会话 serde default 兼容要测试），建议 R3 独立一个迭代、单独回归。

---

## 3. 生态集成（P4-C）

### 3.1 现状（代码证据）

| 维度 | 现状 | 代码定位 |
|------|------|----------|
| SDK | TS/Go/Python 三语言，gRPC+HTTP/SSE 双通道、after_version 续传、端点池 ListMembers | sdk/{ts,go,python}；proto 4 RPC |
| 契约 | openapi 39 paths、proto 4 RPC、storage schema 16+ $defs；**GetItem RPC 服务端死代码**（SDK 全拉全量本地查找） | proto config.v1.proto:19-22；sdk/go/configclient/grpc_client.go:105-116 |
| 测试缺口 | D4 具名用例未自动化（RAFT-002/WCH-002/SDK-002）；D-TEST 断言弱（HTTP version 参数静默丢弃等） | remaining-work.md §3 |
| 部署 | docker-compose.local.yml、CI 8 jobs、SBOM 工作流、cargo-deny | deploy/、.github/ |
| 缺失 | **无 Java SDK / Spring 集成、无 K8s ConfigMap/Secret 控制器、无 Helm chart、无 TLS 内置、无 KMS、无多数据中心** | 报告 §6.4 |

**结论**：与报告一致——"生态集成追赶成本高"。突破口排序：**先 TLS+契约硬化（生产安全基线，1 周内）→ Java SDK + Spring Starter（最大生态杠杆）→ K8s 控制器 + Helm（现代化工作负载入口）**。

### 3.2 阶段计划

| 阶段 | 内容 | 交付物 | 验收 | 估时 |
|------|------|--------|------|------|
| **E0 契约硬化（D-TEST/D4 收尾）** | D4 自动化：RAFT-002 网络分区、WCH-002 慢消费者、SDK-002 幂等重试契约；HTTP version 参数生效或显式拒绝；GetItem RPC 二选一（服务端实现 or 从 proto 移除——建议保留并实现，契约最小化） | 测试脚本 + proto 调整 | 6 个 e2e 全过；新 3 个自动化脚本入 CI | 3–4 天 |
| **E1 TLS 内置** | `--tls-cert/--tls-key`（或自动自签 + 提示）；HTTP/HTTPS 双栈、gRPC TLS、Admin UI HTTPS；SDK 三语言 TLS 选项（Python `tls no-op` 标注收尾）；compose/README 更新 | crypto/证书 + api 监听 + SDK | HTTPS/gRPC-TLS 实测；无证书明文告警 | 4–5 天 |
| **E2 Java SDK + Spring Boot Starter** | `sdk/java`（gRPC+HTTP，复制 Go/TS 模式）+ `dsh-spring-boot-starter`：`@ConfigurationProperties` 注入、`@RefreshScope`、watch→refresh 事件、secret 解密注入 | sdk/java + starter 模块 | Java SDK 契约对拍（复用脚本模式）；starter 集成示例工程启动+热更新实测 | 6–8 天 |
| **E3 K8s ConfigMap/Secret 控制器** | 独立可选二进制：watch (project,branch) → 渲染 → 写 ConfigMap/Secret（ownerRef/label 管理、防回写循环、secret 密文解密按策略）；对存量工作负载零改动 | 新 crate dsh-k8s-sync + manifest | 控制器 e2e（配置变更→ConfigMap 更新→Pod 挂载生效） | 6–8 天 |
| **E4 Helm chart + 运维体系** | `charts/dsh`：StatefulSet（3 节点 Raft）、Headless Service、PV、ingress、TLS secret、安全上下文；多数据中心文档（独立集群 + 跨 DC 只读镜像方案评估） | deploy/charts + docs | `helm install` 3 节点集群实测；README 部署章节更新 | 4–5 天 |
| **E5 成熟度证据** | 压测基准归档（bench.sh 结果入 docs）、k6/ghz 场景脚本、生产案例文档模板 | docs/benchmarks、scripts | 基准数字可复现；CI 基准门槛（可选） | 2–3 天 |
| **E6 KMS 集成（远期）** | 主密钥来源对接 KMS（AWS/GCP/Vault），`--master-key-source=kms:…`；proposal-v4 AC11.2 落地 | crypto 抽象 | 单测 mock KMS；文档 | ≥1 周（远期） |

### 3.3 风险

1. **Spring 生态绑定**：Spring Cloud Config 协议兼容 vs 自研 starter 二选一（建议 starter 优先，协议桥接二期——协议兼容面广但无推送，弱化 Defing watch 优势）。
2. **K8s 控制器职责边界**：ConfigMap 同步器必须防止"应用改 ConfigMap → 控制器回写"死循环（source of truth 只在 Defing）。
3. **TLS 与现有部署兼容**：HTTPS 切换需与 compose/dev-single/集群脚本并存（默认 HTTP，显式开启 TLS），避免破坏现有 e2e。

---

## 4. 统一里程碑与依赖

```
W1  W2  W3  W4  W5  W6  W7  W8  W9  W10 W11 W12
├── G0 ──┬── G1 ──┬── G2 ──────────┬── G3 ──┬── G4 ──┬── G5 ──┐
│        │        │                │        │        │        │
├── R0 ──┼── R1 ──┼── R2 ──────────┼── R3 ──┼── R4 ──────────┤
│        │        │                │        │        │        │
├── E0 ──┼── E1 ──┼── E2 ──┬── E3 ──────────┬── E4 ──┬── E5 ──┤
│        │        │        │                │        │        │
└────────┴────────┴────────┴────────────────┴────────┴────────┘
          D1 旋钮    Action 授权    审批流(交叉)    TLS 是 E 线
          (G1)      (R2)          (R4×G5)        全部前置
```

| 里程碑 | 内容 | 关键依赖 | 退出标准 |
|--------|------|----------|----------|
| M-P4-1（W3 末） | 三线地基：G1 发布策略旋钮 + R1 角色模型 + E0 契约硬化 + E1 TLS | 无 | 4 项 e2e/测试全绿；D1/D4 偏差关闭 |
| M-P4-2（W6 末） | ✅ **G2 灰度状态机完成**（提前于本周）+ R2 action 授权 + E2 Java/Spring | M-P4-1 | G2：core 测试全绿（T1-T8 + 47 用例）、clippy 0 警告、workspace 全绿；R2/E2 待续 |
| M-P4-3（W9 末） | ✅ **G3 数据面灰度完成**（提前）+ R3 多项目 + E3 K8s 控制器 | M-P4-2 | G3：三路解析实测 + watch 隔离实测 + 31 套件全绿；R3/E3 待续 |
| M-P4-4（W12 末） | G4/G5 灰度管理面+放量 + R4 审批流 + E4/E5 Helm+基准 | M-P4-3 | 浏览器全流程；审批闭环；helm 集群实测；基准归档 |

**建议执行节奏**：每里程碑以"测试先行 + e2e 脚本 + 文档落档"三件套收尾（延续仓库纪律）；三线并行时共享依赖（D1 旋钮、授权重构、TLS）优先排期，避免阻塞下游。

## 5. 明确不做（本期，防范围蔓延）

- 灰度：不做"配置层面的流量治理/动态路由"（网关职责）；不做灰度审批自动化决策（只留钩子）。
- RBAC：不做 SSO/OIDC（企业版远期）、不做 item 级权限（成本高收益低，分支级封顶）、不做租户计费。
- 生态：不做 etcd/Consul KV 镜像同步（一致性模型不符）、不做服务注册发现（非配置中心职责，SDK 端点池已覆盖）。
