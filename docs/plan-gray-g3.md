# 开发计划：G3 数据面解析 + watch

> 依据：[design/g3-dataplane.md](design/g3-dataplane.md)（D24-D28 定稿）
> 目标：客户端带身份来、服务器按身份发版本——三处数据面调用点接入 + watch 灰度事件不丢 + proto 字段。

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | `ResolvedVersion{Stable(u64), Gray(u64)}` 枚举 + `resolve_version` 返回类型升级（消除 gray_seq==active 数值歧义，D24） | dsh-core/src/state.rs | 编译 + T2/T7 适配 | | ✅ |
| 2 | `ConfigSnapshot` + `gray`/`resolved_version`（serde default）；`get_config` 普通路径补两字段 | dsh-core/src/state.rs | 编译 + serde 兼容 | | ✅ |
| 3 | `get_config_resolved(id, branch, version, ctx)`：version=0 按身份 resolve + 分流读 v/ 或 gray-snap/；version≠0 显式不 resolve（D27/D28） | dsh-core/src/state.rs | 编译 | | ✅ |
| 4 | proto：GetConfigRequest+instance_id/labels；GetItemRequest 同款；ConfigSnapshot+gray/resolved_version；WatchEvent+gray | proto/config.v1.proto | check-contracts.sh 过 | | ✅ |
| 5 | gRPC get_config/get_item：构造 ClientCtx（字段 + remote_addr IP）→ get_config_resolved → proto 带 gray/resolved_version（D26） | dsh-api/src/grpc.rs | 编译 | | ✅ |
| 6 | gRPC watch：实时过滤 `e.gray \|\| version > last` + WatchEvent.gray（D25） | dsh-api/src/grpc.rs | 编译 | | ✅ |
| 7 | HTTP snapshot：解析 X-Dsh-Instance/X-Dsh-Labels 头 + PeerAddr IP → get_config_resolved → ConfigResp + gray/resolved_version（D26/D27） | dsh-api/src/lib.rs | 编译 | | ✅ |
| 8 | SSE watch：dsh-watch sse_stream 过滤 `e.gray \|\| version > last`（D25）+ 单测 | dsh-watch/src/lib.rs | 全绿 | | ✅ |
| 9 | core 测试：get_config_resolved 三路 + 数值巧合分流 + 显式版本绕过 + ResolvedVersion 适配 | dsh-core/tests/state_machine.rs | 全绿 | | ✅ |
| 10 | gRPC 集成测试：灰度发布 → get_config(身份) 灰度内容 / get_item 同分流 / watch 收 gray 事件 | dsh-api/tests/grpc_data_plane.rs | 全绿 | | ✅ |
| 11 | `scripts/gray-demo.sh`：HTTP 三路 + promote/abort watch 事件端到端 | scripts/ | 退出 0 | | ✅ |
| 12 | 全量回归：workspace 测试 + check-contracts.sh + clippy/fmt | 命令行 | 达标 | | ✅ |
| 13 | 文档收尾：g3-dataplane.md 审核记录 + roadmap-p4.md G3 标记 | docs | 完成 | | ✅ |

## 里程碑

- M1（1-3）：core resolve 升级 + 分流读取（无 wire 面）
- M2（4-8）：proto + gRPC + HTTP + watch 接入
- M3（9-11）：三层测试 + e2e 脚本
- M4（12-13）：回归 + 文档

## 关键纪律

- **B1/N10**：proto 只加字段（proto3 向后兼容），不改既有 RPC 签名；旧 SDK 无身份 = 稳定版（Q2）；
- **D16**：resolve 是读路径纯函数，apply 不读请求——G3 不碰状态机写入；
- **Q4**：watch 过滤加 `e.gray ||` 后，promote/abort 补发事件（gray:true）永不按版本滤掉；
- **Q6**：仅 snapshot/get_config/get_item 三处接身份；render/reveal/diff 明确绕过。

## 风险

- proto 变更 → contract 检查（check-contracts.sh 会把关）；
- gray 事件多推 → 方案 b 契约兜底（SDK 缓存版本号只取 snapshot 响应）；
- gRPC remote_addr 拿不到 → ip=None 兜底（D18：instance_id 优先）。

> **审核处置（2025-08-16，子代理高精度审核：有条件放行）**：
> - 🔴 B1 重放缺口 → SDK 契约（重连必做 snapshot 拉取）闭环，文档化于 design §D25；
> - 🟠 R1 **已改**：灰度响应 `version=active_version`、`resolved_version=gray_seq`（v/ 空间游标正确性）；
> - 🟠 R2 记录性接受：Q2 门闩使纯 IP 规则需 instance_id（文档化 §D26）；
> - 🟠 R3 文档修正：「灰度记录在版本历史中」不成立 → 管理面走 gray-status（§D28）；
> - 🟡 T1-T9 全部处置（proto 注释、文档措辞、合成事件 gray 字段等），详见 design 附录二。
> - 范围增补（审核后）：G3 增加**最小管理面 4 端点**（gray-publish/promote/abort/status +
>   PublishService 3 写方法）——让数据面可被端到端驱动（gray-demo.sh 依赖）；UI tab/openapi 补全仍留 G4。
