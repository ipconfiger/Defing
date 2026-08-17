# 未完成工作清单与下一轮优化计划

> 生成时间：P0–P3 收尾落档 ｜ 依据：docs/progress.md + 上轮源码审计
> 状态：M0–M8 + 后 M8 收尾（P0–P3）+ **后 G5 收口**全部闭环；下列为仍存在的可选增强（非缺陷）

---

## 1. 当前状态快照

| 里程碑 | 状态 | 验证 |
|--------|------|------|
| M0–M8 | ✅ | 74 tests + CI 8 jobs（见 progress.md） |
| P0 管理面契约补全 + 掩码 | ✅ | 11 端点 + api-surface-test（13 断言）+ cluster remove-node |
| P1 SDK gRPC + Admin UI | ✅ | 三语言 gRPC 契约对拍 + 浏览器自动化全流程 |
| P2 CLI admin + watch 增强 | ✅ | admin 8 子命令实测 + SSE after_version 重放/实时验证 |
| P3 指标/测试/stub/deny | ✅ | 76 tests + clippy/fmt + deny.toml |

## 2. 已闭环项（本轮）

- HTTP 管理面 11 个缺失端点（openapi 25→37 paths；含共享库/删除/对比/promote/remove-node）
- secret 掩码策略：管理面/渲染/数据面默认掩码；reveal=true 需会话+审计（gRPC 数据面本就 masked，HTTP 对齐）
- 三语言 SDK gRPC 客户端（GetConfig/GetItem/Watch 续传/ListMembers；Endpoint{grpc?,http?} 双通道）
- Admin UI 管理控制台（登录→草稿→发布→回滚→历史→对比→提升→共享库→审计→watch）
- CLI `dsh admin` 8 子命令（gen/rotate-master-key、force-logout、set-password、promote、remove-node、snapshot、retention-status）
- watch：SSE after_version 续传 + gRPC 慢消费者 snapshot_required；SDK 断线续传
- 指标 2→9 项；LIM-001/AdminSetPassword 测试；dsh-testkit 真实化；cli lib.rs stub 删除；cargo-deny 落地

## 3. 剩余设计偏差（接受/文档化，非缺陷）

| # | 项 | 说明 |
|---|-----|------|
| D1 | CLI 配置旋钮 | ✅ 已闭环（G1 + 后 G5 收口）：--read-mode/--publish-policy/--shared-cascade（G1 三旋钮）+ --watch-event-retain（进程内广播缓冲）+ --allow-no-master-key（启动强制 + 逃生阀，演示脚本已加 flag） |
| D2 | HTTP 数据面无 token 鉴权 | **已闭环（P3）**：`--data-plane-token` 现同时保护 HTTP 数据面 /v1/*（Bearer 或 ?token=，SSE 兼容）与 gRPC；未配置仍开放（演示兼容），生产建议配置 + TLS 前置 |
| D3 | SDK 未实现 leader redirect 跟随 | 数据面读请求任意节点本地可服务（无需转发）；管理面写请求的 ERR_LEADER_REDIRECT 跟随（现返回 428 + leader_hint）属于未来 SDK 管理能力 |
| D4 | 具名用例未全覆盖 | design-v3 §5 的 RAFT-002（网络分区）、WCH-002（慢消费者自动化）、SDK-002（幂等重试契约）未自动化；WCH-002 的语义已实现（F5/D-PRUNED：慢消费者与裁剪起点均结束流并发 snapshot_required），仅缺自动化脚本 |
| D5 | 设备绑定未实现 | device_id 绑定仍未实现（单会话已从机制上收敛并发）；登录限次（--trusted-proxy 后基于对端 IP，不可伪造）+ argon2 已落地 |

## 4. 环境备忘

- macOS 本机构建：`source scripts/build-env.sh`（已自动适配：CI /home 布局或 ~/.cargo 不可写时回退工作区目录；普通机器不覆盖）。存储层已迁纯 Rust redb，无需 RocksDB 时代的 BINDGEN/CXXFLAGS 注入
- Go SDK 需 go ≥1.21（本地可用 .go-toolchain/ 内 go1.22）；grpc 依赖 `go mod tidy`
- TS SDK：`cd sdk/ts && npm install`（@grpc/grpc-js + proto-loader）；Python：`pip install grpcio`
- 端口约定：dev-single 8384/8383；cluster 演示 860x/870x/88xx；api-surface 用 8399
- 端到端脚本：dev-single-demo / cluster-demo / seed-demo（--bootstrap-peers 静态建群）/ chaos-test /
  api-surface-test / sdk-contract-test / sdk-grpc-contract-test / check-contracts
