# 设计文档：G3 数据面解析 + watch（灰度真正生效的一层）

> 状态：待审核 ｜ 基线：main `7279c4b`（G2 已落地）
> 前置：[gray-release.md](gray-release.md) §5.5/§5.6（Q4/Q6 审核修订）、[plan-gray-g2.md](plan-gray-g2.md)（G2 ✅）
> 一句话：**G2 在状态机里"存好"了灰度数据；G3 让数据面"用起来"——客户端带身份来，服务器按身份决定发哪个版本。**

---

## 0. 现状分界线（G2 已就绪 vs G3 缺口）

| 能力 | 状态 | 代码定位 |
|------|------|----------|
| 灰度命令 + 灰度快照存储 + `resolve_version`/`rule_matches`/`fnv1a_hash` | ✅ G2 | state.rs |
| 事件 `gray:bool` + watch 重放还原 gray 标记 | ✅ G2 | model.rs / lib.rs 重放 |
| **HTTP snapshot 认身份** | ⬜ | lib.rs:1783 `snapshot`（恒读稳定版） |
| **gRPC get_config / get_item 认身份** | ⬜ | grpc.rs:131/153（恒读稳定版） |
| **watch 灰度事件不丢** | ⬜ | dsh-watch `e.version > last` 过滤 / grpc watch 同款 |
| proto 身份与 gray 字段 | ⬜ | config.v1.proto |
| 管理面（render/reveal/diff）**不**误接身份 | ⬜ 防御性 | lib.rs render_config 等 |

---

## 1. 核心决策（D24-D28）

### D24：`resolve_version` 返回类型升级为 `ResolvedVersion`（G2 遗留问题的必要修正）

**问题**：G2 的 `resolve_version -> u64` 在 G3 分流时存在**数值歧义**——gray_seq 与 active_version 是独立空间，
数值可能碰巧相等（如结构发布后 active=1，随即 GrayPublish → gray_seq=1）。此时"解析到 1"到底是
读 v/1（稳定）还是 gray-snap/1（灰度）？只看数字无法分流，会读错。

**修订**：返回带语义的枚举：

```rust
pub enum ResolvedVersion { Stable(u64), Gray(u64) }
```

- `Stable(v)` = 客户端读稳定版 v（= active_version）；
- `Gray(seq)` = 客户端读灰度快照 gray-snap/{seq}（= gray_seq）。

`resolve_version` 是 G2 新增的 pub 方法、无外部消费者（仅测试），直接改签名（**非 wire 面，B1 纪律不涉及**）。

### D25：watch 采用**方案 b**（服务端一行，Q4 闭环）

设计 §5.5 给了两个方案。**G3 服务端选 b**：

```
SSE（dsh-watch sse_stream）与 gRPC watch 的实时过滤：
  旧：e.project==p && e.branch==b && e.version > last
  新：e.project==p && e.branch==b && (e.gray || e.version > last)
```

**为什么选 b 而非 a（按身份投递）**：

| 维度 | 方案 a（按身份投递） | 方案 b（gray 事件永不按版本过滤） |
|------|---------------------|----------------------------------|
| 服务端改动 | watch 连接注册身份 + per-subscriber 过滤 + 重放按身份过滤 | 一行 `e.gray \|\|`（SSE+gRPC 各一处） |
| 依赖 | SDK 上报身份给 watch 连接（三语言） | 无（SDK 契约兜底） |
| 旧 SDK 语义 | — | ✅ 天然安全：无身份 → 不进灰度（Q2）→ gray 事件对其本就无意义 |
| 新 SDK 契约 | — | gray:true 事件无条件重拉；缓存版本号只取 snapshot 响应（§5.5 方案 b 原文） |
| promote/abort 补发 | 灰度客户端收 | ✅ gray:true → 不被 `version > last` 滤掉（Q4 闭环） |

**语义自洽论证**：gray 事件只在"有灰度"时产生；有灰度时只有**上报身份的客户端**才可能被 resolve 到灰度版
（Q2：无身份永不进灰度）。因此把 gray 事件无条件推给全部分支订阅者，代价是稳定客户端多收一条
gray 事件并按 SDK 契约触发一次全量重拉（方案 b 的已知代价：灰度活跃期间稳定客户端有额外拉取，
属可接受范围；方案 a 的按身份投递可消除该代价，留作后续增强）。收益是灰度客户端**绝不漏收**
promote/abort 补发事件。方案 a 的带宽优化不作为 G3 阻塞项。

**重放语义**（after_version 续传）：重放基于 version_history 合成事件（lib.rs:3110 / grpc.rs:214），
promote 产生的 VersionRecord `gray=true` 已由 G2 还原标记；重放过滤按 `rec.no > after_version`
（v/ 空间单调）天然正确。**B1（审核阻塞项）：gray publish/abort 不产生 v/ 记录 → 重放列表不含
这两个事件**。这意味着灰度客户端若在 abort 前断线、重连后**收不到撤回事件**，会持续服务已被
撤回的灰度内容（D22/Q4 的反向漏收）。**对策（SDK 契约，B1 闭环）**：watch 重连/订阅后**必须
随后做一次 snapshot 拉取**——服务端按身份 resolve 返回当前状态（abort 后 resolve 回落 active），
缓存版本号只取 snapshot 响应（与方案 b 既有契约一致，天然覆盖此缺口）。

**实现细节（last 游标只增不减）**：
- SSE（dsh-watch）：`last` 是重放末尾固定值（非可变），过滤改为 `(e.gray || e.version > last)` 即可；
- gRPC watch：`last` 随事件更新，gray 事件 version 可能 ≤ last（如 abort 事件 version=回落 active）。
  投递分支内必须 `if e.version > last { last = e.version; }`——**gray 事件投递但不回退游标**，
  否则 last 倒挂会导致后续普通事件重复投递。

### D26：身份注入（数据面三处，Q6）

```
HTTP  snapshot   GET /v1/projects/{p}/branches/{b}/snapshot
                 X-Dsh-Instance: web-1            （SDK 配置，稳定）
                 X-Dsh-Labels: zone=cn-north-1,svc=checkout   （逗号分隔 k=v）
                 IP：PeerAddr（ConnectInfo 注入，lib.rs 已有）

gRPC  get_config / get_item
                 GetConfigRequest.instance_id / labels（map<string,string>，proto3 向后兼容）
                 GetItemRequest  同款字段
                 IP：req.remote_addr()（tonic TcpIncoming 自动注入 RemoteAddr 扩展，免费可用）
```

- **labels 解析**：`k=v,k2=v2` 逗号分隔（SDK 保证值内不含 `,`/`=`）；非法段（无 `=`）跳过；
  重复 key 后者覆盖；空头/空串 → 空身份（走稳定版，Q2）；
- **IP 是兜底**（D18：instance_id 优先 > labels > IP）；gRPC 面 `remote_addr()` 为 None 时 ip=None；
- **无身份 = 稳定版**：旧 SDK/无头请求天然正确（Q2），这是向后兼容的根基。
- **R2（审核修订，记录性）**：Q2 门闩在 `rule_matches` 之前（instance_id 空 → 直接 Stable），
  因此**纯 IP 段规则对无 instance_id 的客户端永不命中**（IP 规则实际要求客户端上报 instance_id；
  典型场景是容器重建时 instance_id 不变、IP 漂移，IP 作为额外判据）。此行为是 Q2 安全门闩的
  直接推论，保留并文档化。

### D27：响应字段（客户端可见自己在哪个版本）

```
core  ConfigSnapshot  + #[serde(default)] gray: bool
                      + #[serde(default)] resolved_version: u64
HTTP ConfigResp       + "gray": true/false
                      + "resolved_version": N
proto ConfigSnapshot  + bool gray = 6
                      + int64 resolved_version = 7
```

**R1（审核修订，关键）字段语义**：灰度命中时
- `version` = **active_version**（v/ 空间）——客户端 watch 游标不错位：
  after_version=active 增量重放正确，避免"灰度序号 < v/ 空间"造成的全量重放放大或 force_snapshot；
- `resolved_version` = **gray_seq**——标记内容实际来自哪个灰度快照；
- `gray` = true——提示客户端自己在灰度。

即 `version` 永远是 v/ 空间单调值（watch 过滤 `e.version > last` 的正确性依赖此点）；
内容来源由 `gray` + `resolved_version` 表达。稳定路径 version=resolved_version=active。

`get_config`（普通路径）设 `gray=false, resolved_version=vno`；灰度路径设 `gray=true, resolved_version=seq`。

### D28：管理面绕过（Q6 明确）

- `render_config` / `reveal` / `branch_diff` / `version_history` **不接身份**（version=0 → 稳定 active 语义不变）；
- `get_config_resolved(version≠0)` **不 resolve**——显式版本号请求（历史/reveal）恒读 v/ 空间；
- 新增 handler 处注释防御，防后续误接。
- **R3（审核修订）**：灰度发布**不产生 v/ 记录**（G2 设计：灰度快照在 gray-snap/ 独立前缀），
  因此"显式传灰度版本号即可读灰度明文"的说法**不成立**（get_config_resolved(N) 对灰度号 → NotFound）。
  管理面查看灰度内容的正确途径是 **gray-status 端点**（G3 已提供状态；灰度内容预览随 G4 UI tab）。

---

## 2. 核心读取路径（get_config_resolved）

```rust
/// 数据面统一入口：version=0 按身份 resolve；version≠0 显式版本（不 resolve，管理面/历史）。
pub fn get_config_resolved(
    &self, id: &ProjectId, branch: &BranchName, version: u64, ctx: &ClientCtx,
) -> Result<ConfigSnapshot, Error> {
    if version != 0 {
        return self.get_config(id, branch, version); // 显式：恒走 v/ 空间
    }
    match self.resolve_version(id, branch, ctx)? {
        ResolvedVersion::Stable(_) => self.get_config(id, branch, 0),   // 普通路径 gray=false
        ResolvedVersion::Gray(seq) => {
            let snap = self.gray_snapshot_of(id, branch, seq)?;         // 读 gray-snap/
            let structure = self.get_structure(id)?.unwrap_or(...);
            // R1：version=active_version（v/ 空间），resolved_version=seq（灰度来源）
            Ok(ConfigSnapshot { version: st.active_version, gray: true,
                resolved_version: seq, groups: snap, ... })
        }
    }
}
```

`resolve_version` 内部：无灰度 / 规则 None / 无身份（Q2）→ `Stable(active)`；
规则命中 → `Gray(gray_seq)`；未命中 → `Stable(active)`。

---

## 3. 代码改动清单

| 文件 | 改动 | 影响 |
|------|------|------|
| `dsh-core/src/state.rs` | `ResolvedVersion` 枚举；`resolve_version` 返回类型升级；`ConfigSnapshot` + `gray`/`resolved_version`（serde default）；`get_config` 补两字段；新增 `get_config_resolved`（R1：version=active、resolved_version=gray_seq） | 纯新增 + 方法签名升级（无 wire 面） |
| `dsh-core/tests/state_machine.rs` | 适配 `ResolvedVersion`（T2/T7）；新增 `get_config_resolved` 三路 + 数值巧合分流 + 显式版本绕过 | 测试 |
| `dsh-watch/src/lib.rs` | `sse_stream` 实时过滤 `e.version > last` → `(e.gray \|\| e.version > last)`；单测 gray 事件绕过过滤 | 行为微调（灰色事件多推） |
| `proto/config.v1.proto` | `GetConfigRequest`+instance_id(4)/labels(5)；`GetItemRequest`+instance_id(6)/labels(7)；`ConfigSnapshot`+gray(6)/resolved_version(7)；`WatchEvent`+gray(8)；修正 version 注释（gray 事件可 version≤last） | proto3 加字段，向后兼容 |
| `dsh-publish/src/lib.rs` | 新增 `gray_publish`/`gray_promote`/`gray_abort` 写路径（dev-single/集群一致） | 管理面写 |
| `dsh-api/src/grpc.rs` | get_config/get_item：构造 ClientCtx（字段 + remote_addr IP）→ `get_config_resolved` → proto 带 gray/resolved_version；watch 实时过滤加 `e.gray \|\|` + WatchEvent.gray + last 只增不减 | 数据面行为 |
| `dsh-api/src/lib.rs` | `snapshot` handler：解析身份头 → `get_config_resolved` → ConfigResp + gray/resolved_version；**G3 最小管理面**：`POST …/gray-publish`/`gray-promote`/`gray-abort` + `GET …/gray-status`（4 端点 + 审计 action；UI tab 与 openapi 补全留 G4）；render/reveal 加"不接身份"注释防御 | 数据面 + 管理面 |
| `dsh-api/tests/grpc_data_plane.rs` | 灰度场景：gray publish → get_config(instance_id) 灰度内容 / get_item 同分流 / watch 收 gray 事件 | 集成测试 |
| `scripts/gray-demo.sh` | 端到端 curl 演示：华北/华南/无身份三路 + promote 补发事件 | e2e 脚本 |
| `docs/roadmap-p4.md` / `plan-gray-g3.md` | 状态标记 + 审核处置 | 文档 |

**明确不做（本期）**：方案 a 按身份投递 watch（带宽优化，后续）；SDK 三语言适配（G3/G4 同步，服务端就绪后每语言 1-2 天）；
灰度快照回收策略（G4+）；自动回滚钩子（G5）。

---

## 4. 测试计划

| 层 | 用例 | 验收 |
|----|------|------|
| core | `get_config_resolved`：标签命中→Gray 快照内容 / 未命中→稳定 / 无身份→稳定 / **gray_seq==active 数值巧合时读对快照** / version≠0 显式不 resolve | 全绿 |
| core | `resolve_version` 返回 `ResolvedVersion`（T2/T7 适配） | 全绿 |
| watch | dsh-watch 单测：gray:true 且 version ≤ last 仍推送（Q4）；gray:false 且 version ≤ last 不推送 | 全绿 |
| gRPC | grpc_data_plane.rs：gray publish → get_config(instance_id=web-1, labels=zone=cn-north-1) 返回灰度内容 + gray=true；无身份返回稳定 + gray=false；get_item 同分流；watch 订阅收到 promote 的 gray 事件 | 全绿 |
| e2e | `scripts/gray-demo.sh`：HTTP 三路解析 + watch 事件流（gray publish → promote → abort 全链路） | 脚本退出 0 |
| 回归 | `cargo test --workspace` + `check-contracts.sh`（proto 变更后） + clippy/fmt | 全绿 |

---

## 5. 风险与对策

| 风险 | 对策 |
|------|------|
| proto 加字段破坏旧 SDK | proto3 加字段天然向后兼容；旧 SDK 不传身份 → 空身份 → 稳定版（Q2）；contract 检查把关 |
| gray 事件多推造成稳定客户端误重拉 | 方案 b 契约：SDK 缓存版本号只从 snapshot 响应更新；服务端无状态损失，仅一条冗余事件 |
| `remote_addr()` 在部分部署拿不到 IP | ip=None 兜底（D18：instance_id/labels 优先，IP 本来就是兜底） |
| resolve 每请求读分支状态的开销 | get_config_resolved 单次读；G5 若需可加缓存（本期不做） |

---

## 附录一：决策记录（D24-D28）

| 决策 | 结论 | 一句话理由 |
|------|------|-----------|
| D24 resolve 返回类型 | `ResolvedVersion{Stable(u64), Gray(u64)}` | gray_seq 与 active_version 数值可能巧合，裸数字无法分流 |
| D25 watch 方案 | 方案 b（gray 事件永不按版本过滤） | 服务端一行、旧 SDK 天然安全、promote/abort 补发不丢（Q4） |
| D26 身份注入 | HTTP 头 + gRPC 字段 + remote_addr IP | Q6 三处调用点；无身份 = 稳定（Q2 向后兼容根基） |
| D27 响应字段 | gray + resolved_version 三层同步 | 客户端可见自己在哪个版本（§5.6） |
| D28 管理面绕过 | render/reveal/diff 不接身份；显式版本不 resolve | 管理员看稳定客户端所见（Q6） |

## 附录二：审核记录（2025-08-16，子代理高精度审核）

**结论：有条件放行**——1 🔴 阻塞 + 3 🟠 修订 + 9 🟡 提示；全部处置如下。

| # | 审核问题 | 严重度 | 处置 |
|---|---------|--------|------|
| B1 | **重放缺口**：gray publish/abort 不写 v/ 记录 → watch 重放不含这两个事件；灰度客户端 abort 前断线、重连后收不到撤回事件，持续服务已撤回的灰度内容（Q4 反向漏收） | 🔴 阻塞 | ✅ **SDK 契约闭环**（§D25 重放语义）：watch 重连/订阅后必须做一次 snapshot 拉取（resolve 返回当前状态，abort 后回落 active）；缓存版本号只取 snapshot 响应（与方案 b 既有契约一致） |
| R1 | **D27 语义问题**：灰度响应 `version=gray_seq` 使客户端缓存离开 v/ 空间 → 重连 after_version=灰度号导致全量重放放大/force_snapshot、`version==after_version` 碰撞时普通 v/ 事件被 `e.version>last` 静默滤掉 | 🟠 修订 | ✅ **已改**：灰度命中 `version=active_version`（v/ 空间）、`resolved_version=gray_seq`、`gray=true`；G3-D1 测试与文档同步 |
| R2 | **Q2 门闩 vs IP 规则**：instance_id 空 → 直接 Stable，纯 IP 段规则对无 instance_id 客户端永不命中 | 🟠 修订 | ✅ 记录性接受（§D26）：Q2 安全门闩的直接推论；IP 规则实际要求上报 instance_id（容器重建场景 instance_id 不变、IP 兜底）；文档化 |
| R3 | **「灰度记录在版本历史中」不成立**：灰度发布不产生 v/ 记录，`get_config_resolved(灰度号)` → NotFound | 🟠 修订 | ✅ 文档修正（§D28）：管理面看灰度内容走 gray-status 端点；灰度内容预览随 G4 UI tab |
| T1 | dsh-watch 注释过度自信（SSE 无游标倒挂） | 🟡 | ✅ 注释已限定"`last` 为重放末尾固定值（非可变）" |
| T2 | openapi.yaml 未列入改动清单 | 🟡 | ✅ 明确：openapi 补全归 G4（contract 检查不校验路由对拍） |
| T3 | WatchEvent.version "单调递增"注释失实（gray 事件可 version≤last） | 🟡 | ✅ proto 注释已修正 |
| T4 | labels 解析 `split_once('=')` 与文档边界 | 🟡 | ✅ 文档已明确 SDK 保证值内不含 `,`/`=`；非法段跳过 |
| T5 | get_item 响应无灰度信息 | 🟡 | ⏳ 记录：ItemValue 加 gray 标记随 G4（不影响正确性——值已按身份分流） |
| T6 | 稳定客户端每次灰度事件触发一次全量重拉（非"一条冗余事件"） | 🟡 | ✅ 文档措辞已修正（§D25 语义自洽论证） |
| T7 | gray 快照缺失无降级 | 🟡 | ⏳ 记录：Q5 保证 prune 不裁 gray-snap；异常缺失 → NotFound（可观测） |
| T8 | 合成 snapshot_required 无 gray 字段 | 🟡 | ✅ 已实现 gray=false |
| T9 | 审核时工作树处于编译失败中间态（GrayRule 未导入） | 🟡 | ✅ 已修复并全量回归 |

**正面确认**：D24 数值巧合论证与测试构造正确；D25 两处实时过滤同步实现（gRPC 侧 last 只增不减保护）；D26 三处注入落地（PeerAddr/ConnectInfo/remote_addr）；D28 调用点全量核查无遗漏；proto/tonic/contract 无破口；G2 兼容声明成立。

