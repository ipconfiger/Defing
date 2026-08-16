# 开发计划：多会话并存 + 每会话独立管理

> 依据：[design/multisession.md](design/multisession.md)

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | keys.rs：session_key_with / pa_session_key_with / K_SESSION_PREFIX | dsh-core/src/keys.rs | 编译 |
| 2 | command.rs：6 命令加 `session_id`（serde default） | dsh-core/src/command.rs | 编译 |
| 3 | state.rs：apply 分支（按 sid）+ 级联批量删 + 访问器 | dsh-core/src/state.rs | 编译 |
| 4 | lib.rs：token 生成（带 sid）+ resolve_principal 路由 | dsh-api/src/lib.rs | 编译 |
| 5 | lib.rs：login/pa_login（去 409）+ logout/heartbeat（按 sid）+ force-logout（单个/批量） | dsh-api/src/lib.rs | 编译 |
| 6 | 测试 T1-T7 新增 + 会话相关用例适配 | dsh-core tests + dsh-api tests | 全绿 |
| 7 | 全量测试 + e2e + 多会话实测 | 命令行 | 达标 |
| 8 | 更新 research-multisession.md / multisession.md 状态 | docs | 标记完成 |

## 里程碑

- M1（1-3）：core 层，`cargo test -p dsh-core` 绿
- M2（4-5）：api 层
- M3（6-7）：测试 + 实测
- M4（8）：文档收尾

## 风险

- Raft wire 兼容（旧日志无 session_id）：serde default 空串 → 旧语义，T6 覆盖
- resolve_principal 解析安全性（token 段数检查防绕过）：T6 + 负例
- 级联批量删（改密/删号）：前缀扫描正确性，T7 覆盖
- Principal 不改形状（sid 经 Authorization 头重解析）：logout/heartbeat 一致性
