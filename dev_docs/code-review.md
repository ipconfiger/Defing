# 代码审查报告（高精度复核版）

> 审查日期：2025-08-16
> 审查范围：`server/crates/*`（12 个 crate，约 13.7k 行）、`dsh-api/admin/index.html`（Admin UI）、
> `sdk/ts`、`sdk/python`、`sdk/go`、部署脚本（`scripts/`、`deploy/`、`docker-compose.local.yml`）。
> 验证手段：`cargo clippy --workspace --all-targets`（0 告警）、`cargo test --workspace`（全部通过）、
> 逐项源码证据链复核（本文件结论均经过二次核对，误判项已排除并注明）。
>
> 关联文档：[remaining-work.md](remaining-work.md)（既有 D1–D5 偏差清单）、[progress.md](progress.md)。

---

## 0. 结论摘要

| 类别 | 确认必须解决 | 复核后降级/排除 |
|------|-------------|----------------|
| 安全 | S1–S5（5 项） | S7 降为低优先；S8 排除（实际不可利用） |
| 正确性 | C1–C3（3 项） | C4 降为低（前置条件收窄）；C5 修正为仅 HTTP 且降为低；C6 降为低 |
| 可靠性 | R1（1 项） | R2 降为低（规模相关）；R3 为已知设计 |
| 运维 | O1（1 项） | O2/O3/O4 为低优先提示 |

**必须解决 Top 7（按优先级）**：S1 XSS、S2 join 无鉴权、S3 主密钥轮换不跨节点、C1 operator="test"、
C2 集群时间戳=日志序号、S4 ring 文件 0644、R1 raft RPC 无超时。

---

## 1. 安全维度（已确认）

### S1【严重】Admin UI 存储型 XSS → 项目管理员可劫持全局管理员会话

**证据链（全链路复核通过）**：

1. **分组名/键名无字符集校验**（可含 `<img onerror=...>` 等 HTML）：
   - `dsh-core/src/validator.rs:71-91` `validate_structure` 只校验长度上限、重复、secret 类型，**不校验字符集**；
   - `dsh-core/src/state.rs:1052-1068` `apply_shared_draft_update` 只校验 group/key 非空。
2. **PA 可写入恶意分组名**：`dsh-api/src/lib.rs:392-408` `pa_allowed` 对 `/api/v1/projects/{p}/structure-draft`（PUT）与
   `.../structure-draft/publish`（POST）放行（项目本地端点），即项目管理员可发布含恶意分组名的结构。
3. **Admin UI 未转义直接拼 HTML**（`dsh-api/admin/index.html`）：
   - `:327` `<b>${g}</b>`（分组名）；
   - `:333` `<td class="mono">${k}</td>` 与 `onclick="delDraftItem('${g}','${k}')"`（JS 字符串注入）；
   - `:416` `${x.group}/${x.key}`（diff 表）；
   - `:450` `${s.group}` / `${s.key}`（共享库表）。
   - 对照：`:332/:349/:399/:487` 对**值**与**comment** 用了 `esc()`，唯独 group/key 未转义。
4. **CSP 不构成缓解**：`security_headers`（`lib.rs:292-313`）中 `script-src 'self' 'unsafe-inline'`，内联脚本本就放行。

**攻击链**：PA 建结构草稿（分组名含 payload）→ 发布结构 → 写入该组草稿值 → 全局管理员打开控制台
（`branch_detail` 返回的草稿组名直接进 `renderDraftEditor`）→ 脚本在管理员会话执行 →
读取 `localStorage.dsh_admin_token` → 完全提权。

**修复**：① 服务端统一校验 group/key/共享项名字符集（如 `[A-Za-z0-9._-]{1,128}`）；② 前端所有插值统一 `esc()`；
③ CSP 移除 `'unsafe-inline'`（脚本外置）。

---

### S2【高】`/api/v1/cluster/join` 完全无鉴权 → 任意人拉走全量状态

**证据**：`dsh-api/src/lib.rs:422-426` `auth_middleware` 将 `/api/v1/cluster/join` 整体豁免；
`lib.rs:2314-2342` `cluster_join` 直接 `raft.add_learner(req.node_id, node, false)`，不校验任何身份、
不校验 `node_id` 唯一性。`main.rs:289-317` 客户端 30s 内重试直到 leader 响应。

**影响**：能触达 HTTP 管理端口者即可把自己注册为 learner → leader 向其复制全量 Raft 日志/快照，
内容包括：全部配置密文、**审计记录、管理员/PA 密码哈希（`sess/admin-pw`、`adm/pa/*`）、会话 token 哈希**（可离线爆破，放大 S6）；
重复 `node_id` 加入可扰乱集群。注：learner 不能投票、不能自行 promote（promote 需管理员会话），
故威胁面为**数据泄露 + 集群扰乱**，非直接接管。

**修复**：join 要求共享密钥（`--join-token`）或一次性引导凭证；至少校验 `node_id` 未占用、地址可达。

---

### S3【高】主密钥轮换在集群中不一致（仅单节点生效，不经过 Raft）

**证据**：`dsh-api/src/lib.rs:2436-2473` `rotate_master_key`：
- 只改**请求到达节点**进程内 `Cipher` 的 keyring（`dsh-crypto/src/lib.rs:98-100` `rotate_master_key` push 新 KEK）；
- 只写**该节点**本地 ring 文件（`lib.rs:2454-2457`）；
- `Command` 枚举（`dsh-core/src/command.rs`）与 Raft apply 路径（`dsh-raft/src/store.rs`）均**无 ring 复制逻辑**；
- `main.rs:350-364` 各节点启动时从各自 `--master-key-file` + 本地 ring 文件独立加载。

**影响**：
1. 轮换后 leader 故障转移到未轮换节点 → 该节点 keyring 无新 KEK → `decrypt_secret` 的 fallback 循环
   （`crypto/lib.rs:172-179`）只尝试 1..=旧代际 → **轮换后写入的 secret 全部不可解**（reveal 变 `***`、级联数据不可用）；
2. 代码先内存轮换、后存文件（`lib.rs:2452` 在 `:2454` 之前）：`save_ring` 失败时请求报错但内存已切换，
   重启后新密文永久不可解；
3. 附：仅用 `DSH_MASTER_KEY`（无 ring 文件）时轮换状态无任何持久化，重启即丢。

**修复**：KEK 轮换改为 Raft Command 复制（ring 变更进状态机）或强制全节点同步 ring 文件后重启；
先持久化成功、再切换内存。

---

### S4【高】主密钥环文件权限 0644（世界可读）

**证据**：`dsh-crypto/src/lib.rs:210-216` `save_ring` 用 `std::fs::write`（默认 0666 & umask → 通常 0644），
文件内容为**全部历史 + 当前 KEK 的 base64 明文**（`crypto/lib.rs:211`）。

**影响**：多用户主机上任何本地用户可读 → 解密全部 secret 项，整套信封加密失效。

**修复**：写入后 `chmod 0600`（并校验主密钥文件本身权限，读时告警）。

---

### S5【中】Raft RPC 端点 `/raft/*` 无鉴权

**证据**：`dsh-raft/src/raft_http_server.rs:21-60` 三个端点（append-entries / vote / install-snapshot）
无任何鉴权中间件；`dsh-raft/src/http_network.rs:26-47` 客户端亦无凭证。

**影响**：能触达 `raft_addr`（默认 8385）者可发送高任期 vote/append-entries 制造选举抖动（leader 退位 DoS）；
伪造 `install_snapshot`（meta 需与日志衔接，有一定校验）存在状态注入面。

**缓解现状**：raft 端口通常仅内网可达；属纵深防御缺口，非独立可利用入口。

**修复**：raft 端口绑定内网网卡 + 共享 token/mTLS。

---

### S6【中】登录无限次节流 + 密码哈希为 SHA-256 快哈希

**证据**：`dsh-api/src/lib.rs:1805-1970`（login 无任何节流/锁定）；`lib.rs:1440-1450`
`salted_password_hash` = `sha256(salt||password)`；`dsh-core/src/state.rs:1652-1660` `token_hash` = SHA-256。

**影响**：经 S2（join 拉取状态）获取哈希后**可离线爆破**；在线暴力破解无节流。
此条在 `remaining-work.md` D5 已登记为已知偏差（"登录限次/设备绑定未实现"），故列为必须解决的下位项。

**修复**：登录限次（如 5 次/10min 锁定）+ 指数退避；PA/admin 密码哈希升级 argon2/bcrypt。

---

### S7【低】`/metrics` 无鉴权且泄露 `session_active`

**证据**：`dsh-api/src/lib.rs:2236-2254` `/metrics` 不属 `/api/v1/*`，无鉴权；
`dsh-observability/src/lib.rs:133-140` 输出 `dsh_session_active`（会话存在性 oracle）。
低优先：属信息泄露辅助面，与 S6 组合使用。

---

## 2. 正确性维度（已确认）

### C1【高】dsh-publish 全部写命令硬编码 `operator: "test"`

**证据**：`dsh-publish/src/lib.rs`：
- `:124` `update_draft`、`:149` `publish`、`:189` `rollback`、`:219` `publish_structure`
  四处写 `operator: "test".to_string()`，参数名为 `_operator`（被丢弃）；
- 调用方 `dsh-api/src/lib.rs:672/1024` 等已正确传入 `principal_op(&principal)`；
- 状态机 `state.rs:962` 将 operator 落进 `VersionRecord.operator`。

**影响**：`version_history` 返回的**版本记录 operator 恒为 "test"**（dev-single 与集群均如此），
身份字段不可信；审计条目本身正确（API 层单独走 `AuditLog`），故为"数据质量 + 审计一致性"缺陷，
并直接违背近期 commit 声称的"审计 operator 贯穿全部写路径"。

**修复**：`_operator` 改名并透传。

---

### C2【高】集群模式下所有时间戳 = Raft 日志序号（显示 1970 年）

**证据**：`dsh-raft/src/store.rs:544` `apply` 中 `let now_ms = log_id.index as i64;`
传入状态机；而 dev-single 路径（`dsh-raft/src/raft.rs:169-173`）用 API 层墙钟 `now_ms()`。
`write_command` 集群分支（`raft.rs:182-213`）**不使用**传入的 `now_ms`。

**影响**：集群模式下 `Project.created_at`、版本 `created_at`、草稿 `updated_at`、PA 账号 `created_at`
全部为日志序号（如 42 → UI 显示 1970-01-01）；与 dev-single 数据不一致。
会话 `issued_at/expires_at`（API 层注入）与审计 `ts` 不受影响（正确）。

**修复**：命令载荷携带墙钟（照 `SessionLogin.issued_at` 做法），apply 只用载荷时间；或 leader 提交前注入。

---

### C3【中】共享项 group/key 无格式校验 → 键注入与级联静默失效

**证据**：`state.rs:1052-1068` 只查非空；`dsh-core/src/keys.rs:69-71` `shared_key = "sh/{group}/{key}"`，
`keys.rs:96-98` 引用索引 `idx/ref/{sg}/{sk}/{project}/{group}/{item_key}` 以 `/` 分隔；
`state.rs:1156-1167` 解析索引时 `parts.len() != 3 → continue`（静默跳过级联）。

**影响**：共享 group/key 含 `/` 时：
1. 索引键错位 → 发布共享项后**级联静默不生效**（无任何报错）；
2. 键碰撞（`sh/a/b` 与 `sh/a` + key `b` 歧义）；
3. 大写/控制字符绕过前端展示预期（并放大 S1 XSS 面）。

**修复**：共享 group/key 复用项目名校验规则（`[a-z0-9][a-z0-9-]{0,127}`）；索引解析改可区分编码。

---

## 3. 可靠性维度（已确认）

### R1【高】Raft HTTP 传输无请求超时 → 黑洞节点拖停复制

**证据**：`dsh-raft/src/http_network.rs:96-106` `reqwest::Client::new()`（无 connect/total 超时）；
`:26-47` `post()` 直接 `.send().await` 且忽略 `RPCOption` 中的超时；全仓 grep 无任何 `timeout` 设置。

**影响**：对端黑洞（SYN 丢弃 / 连接黑化）时 `append_entries`/`vote` 可挂起至 OS TCP 超时（分钟级甚至更长），
期间 Raft 复制停滞；与 leader 心跳互斥叠加可致选举抖动。

**修复**：Client builder 设置 connect + total timeout（如 3s/10s），并消费 `RPCOption` 的超时。

---

## 4. 复核后降级 / 排除的项（原审误判修正）

| 原编号 | 原结论 | 复核结论 | 依据 |
|--------|--------|----------|------|
| S8 | token 比较非常数时间 | **排除（非必须）** | 64 位 hex SHA-256 经网络抖动做时序攻击不可行；仅本地缓解价值，不构成必须项 |
| C4 | item 级引用级联不校验目标分支结构 | **降为低** | 结构是项目级（`get_structure(project)`），绑定时刻已校验 item 在结构内（`state.rs:1295-1330`）；仅在"绑定后结构重新发布移除该 item"的窄前置条件下，级联才插入结构外键 |
| C5 | HTTP/gRPC watch 重放静默吞错 | **修正 + 降为低** | gRPC 路径 `grpc.rs:204-211` 用 `map_err(...)?` **会传播错误**；仅 HTTP `lib.rs:2647-2673` 静默吞错。属低危（存储错误罕见；裁剪后重放语义已文档化接受） |
| C6 | restore_all 非原子 | **降为低** | 仅 raft `install_snapshot` 调用；崩溃窗口有 `last_applied` 兜底（`store.rs:596-598` 后写），raft 会重装/重放；且当前 CLI 无 restore 命令（`admin snapshot` 只导出） |
| R2 | /metrics 持锁全表扫描阻塞 apply | **降为低** | 属实但规模相关；中小库单次抓取毫秒级。建议后续加缓存，非必须 |
| R3 | 审计"尽力而为"丢条目 | **降为已知设计** | 代码注释即"尽力而为"（`observability.rs:11`），MVP 取舍；合规场景再升级队列重试 |
| O1 | build-env.sh 硬编码 /home 路径 | **降为低（环境相关）** | 脚本头部声明"本机构建环境"（CI 的 /home 只读布局）；本机 macOS 下 README 指引会失败，但可能适配于 CI。建议改为自动探测 |
| O4 | 文档/实现漂移 | **确认（低）** | 复核确认：`limits.rs:3` "均可在启动参数覆盖"无对应参数；`MAX_PROJECTS`/`CHECKPOINT_INTERVAL` 全仓无使用（死常量）；`ErrorKind::VersionPruned` 仅定义于 `error.rs:15,37`，无任何产生路径 |
| S7 | /metrics 泄露 session_active | **确认（降为低）** | 属实；信息泄露辅助面 |

---

## 5. 复核中新增的低优先项（顺带记录）

| 编号 | 项 | 说明 |
|------|-----|------|
| N1 | `apply_project_delete`（`state.rs:574-591`）未清理 `idx/ref/*`、`idx/refg/*` 引用索引 | 删除项目后留孤儿索引；级联时对已删项目 `list_branches` 返回空 → 无害 no-op，但索引脏数据残留 |
| N2 | 版本恒为 Full 快照 + `--version-retention` 默认 0（全量保留） | `keys.rs` 注释称 M2 起按 checkpoint 存 diff，实际 `VersionKind::Full` 恒用（`model.rs:256-261`）；`CHECKPOINT_INTERVAL` 死常量。长期运行磁盘线性增长，建议默认开启保留策略 |
| N3 | `main.rs:355-358` `load_ring(...).ok()` 静默吞错 | ring 文件损坏 → 当空处理 → 旧密文不可解且无告警 |
| N4 | 仅 env 密钥 + 轮换 → 无 ring 文件可持久化 | 重启后新 KEK 丢失（S3 的变体） |

---

## 6. 与既有文档对齐

- `remaining-work.md` D2（HTTP 数据面无 token 鉴权）、D5（登录限次未实现）→ 本报告 S6 对齐，维持"已知偏差"定位；
- `remaining-work.md` D4（具名用例未全覆盖）→ 不影响本报告结论；
- `progress.md` 声称"审计 operator 贯穿全部写路径" → 与 C1 冲突，**该声明在版本记录层面不成立**，需修复后更新文档。

---

## 7. 修复优先级建议

| 优先级 | 项 | 预计改动面 |
|--------|-----|-----------|
| P0 | S1 Admin UI XSS | 服务端校验 + 前端 esc（小） |
| P0 | S2 join 鉴权 | 加 `--join-token`（小） |
| P0 | S3 主密钥轮换集群一致性 | 设计改动（中-大，需 Raft Command 或运维流程） |
| P0 | C1 operator 透传 | 改 4 处参数名（极小） |
| P1 | C2 时间戳注入 | 命令载荷加时间字段（中） |
| P1 | S4 ring 文件 0600 | 1 行（小） |
| P1 | R1 raft RPC 超时 | Client builder 配置（小） |
| P1 | S5 raft 端点鉴权 | 配置项 + 中间件（中） |
| P2 | C3 共享键校验 / S6 登录加固 / S7 / N1-N4 | 按需 |

建议先落地 P0 四项（其中 C1、S4 为 5 分钟内可完成的改动），再排期 P1。

---

## 8. 修复状态（2025-08-16 多 subagent 并发实施）

> 实施方式：按文件所有权分 5 波次（可并发的并发、共享文件的串行），7 个 subagent 独立修复，
> 波次间人工编译验证；最终 `cargo test --workspace` 115 用例全绿、`cargo clippy --workspace --all-targets` 零告警，
> 并做了 dev-single 端到端冒烟（时间戳/operator/XSS 校验/共享键校验/轮换/ring 0600/重启持久化）。

| 项 | 状态 | 实施要点 |
|----|------|----------|
| S1 Admin UI XSS | ✅ 已修复 | 服务端 `validator.rs::valid_key_name` 字符集封死（结构分组名/item 键/共享项/引用绑定）+ 前端 `esc()` 全量补齐（含审计 request_id、diff、共享表、onclick 注入） |
| S2 cluster/join 无鉴权 | ✅ 已修复 | `ApiState.join_token` + `cluster_join` 校验 Bearer（`join_token_ok` 单元测试）；CLI `--join-token`；join_cluster 客户端携带 |
| S3 主密钥轮换不一致 | ✅ 已修复 | `Command::RotateMasterKey` 经 Raft 复制 + `StateMachineStore` rotation 钩子（幂等、持久化失败不切内存、重放安全）；dev-single 改为先持久化后切换；CLI `cluster_rotation_hook` 接线；另修复 ring 加载重复插入文件密钥的既有 bug（每次重启代际 +1） |
| S4 ring 文件 0600 | ✅ 已修复 | `save_ring` OpenOptions mode 0o600 + set_permissions 兜底（顺带修复存量 0644 旧文件）；`#[cfg(unix)]` 权限断言测试 |
| S5 raft RPC 无鉴权 | ✅ 已修复 | `RaftServerState::with_token` + 三端点 `authed` 校验；`HttpNetworkFactory::with_token` 客户端携带；CLI `--raft-token` 接线（默认关闭，行为不变） |
| R1 raft RPC 无超时 | ✅ 已修复 | reqwest builder `connect_timeout(3s)` + `timeout(60s)` |
| C1 operator="test" | ✅ 已修复 | `dsh-publish` 四方法 `_operator` → `operator` 透传；运行时验证版本记录 operator="admin" |
| C2 集群时间戳=日志序号 | ✅ 已修复 | 8 个时间戳命令变体新增 `#[serde(default)] ts: i64`（API 层注入墙钟，0 回退 now_ms 兼容旧日志）；`eff_ts` 统一解析；运行时验证 created_at 为真实墙钟 |
| C3 共享键注入/级联失效 | ✅ 已修复 | 共享 group/key 与引用绑定接入 `valid_key_name`（拒绝 `/` 与 HTML 特殊字符） |
| S6 登录限次 + 密码哈希 | ⏳ 未实施（已知偏差 D5） | 建议后续：登录节流 + argon2/bcrypt |
| S7 /metrics 泄露 session_active | ⏳ 未实施（低优先） | 建议后续：指标鉴权或去除该指标 |
| N1–N4 / O1–O4 | ⏳ 未实施（低优先） | 见第 5 节清单；O1（build-env.sh 路径）建议改自动探测 |

**验证证据**：dev-single 冒烟——① 项目/版本 `created_at > 1e12`（真实墙钟）；② `version_history` operator='admin'；
③ 结构草稿含 `<img onerror=…>` 分组名 → HTTP 422；④ 共享项 `group="a/b"` → ERR_VALIDATION；
⑤ 连续轮换 generation 1→2→3，重启后 ring 文件项数=去重后=3（无重复），再轮换正常；⑥ ring 文件权限 `-rw-------`。

### 第 2 轮（2025-08-16 续）：S6 登录加固 + N1/N3/N4

| 项 | 状态 | 实施要点 |
|----|------|----------|
| S6a 登录节流 | ✅ 已修复 | `ApiState.login_throttle` 进程内固定窗口（600s/5 次失败，按 X-Forwarded-For 首值，缺省 "direct"）；超限 429 `ERR_TOO_MANY_ATTEMPTS`；成功登录 reset。单测 3 个 |
| S6b 密码哈希升级 | ✅ 已修复 | `argon2 = "0.5"` 依赖；`hash_password` 产出 PHC 字符串（盐内嵌、salt 字段置空）；`verify_password` 兼容 legacy `sha256(salt‖pw)`（存量密码在改密前仍可用）；admin 与 PA 登录/设密全部切换。单测 2 个 |
| N4 轮换守卫 | ✅ 已修复 | 无 ring 文件（仅环境变量密钥）时拒绝轮换：`ERR_VALIDATION "主密钥轮换需要 --master-key-file"`，杜绝"内存轮换、重启丢新 KEK" |
| N1 孤儿引用索引 | ✅ 已修复 | `apply_project_delete` 清理 `idx/ref/{sg}/{sk}/{project}/…` 与 `idx/refg/{sg}/{project}/{group}`（按段位匹配 project）；单测 1 个 |
| N3 ring 加载静默吞错 | ✅ 已修复 | main.rs `load_ring` 失败改 `tracing::warn!`（不再静默当空） |
| S7 / N2 / O1–O4 | ⏳ 未实施（低优先/已知偏差） | S7 metrics session_active；N2 版本保留默认 0（磁盘增长）；O1 build-env.sh 路径；O2 Dockerfile root；O3 compose 开发密钥；O4 文档漂移 |

**第 2 轮验证**：`cargo fmt --check` PASS、`cargo clippy --all-targets --all-features -D warnings` 0 告警、
`cargo test --workspace` **121 用例全绿**（+6）；冒烟——连续 5 次错密码 401→第 6 次 429（含正确密码也被锁，固定窗口语义）；
环境变量密钥下轮换返回 ERR_VALIDATION；PA 账号存储 `$argon2id$v=19…` PHC（salt 字段空）、正确密码 200 / 错误 401。

### 第 3 轮（2025-08-16 续）：低优先项复核与实施

复核结论：S7 / N2（限额部分）/ O1 / O3 / O4 对当前实现无运行影响 → 已实施；
O2 与 N2 的"默认开启版本保留"会影响现有行为 → 按"有影响则不做"原则跳过并说明。

| 项 | 状态 | 实施要点 |
|----|------|----------|
| S7 metrics 泄露 session_active | ✅ 已修复 | 移除 `dsh_session_active` 指标（`metrics_text` 去掉该参数 + api handler 去掉聚合计算 + 顺带删除已无调用方的 `StateMachine::any_pa_session_active`）；`/metrics` 实测不再输出，其余指标不变 |
| N2 限额强制 | ✅ 已修复 | `apply_project_create` 增加 `MAX_PROJECTS` 上限强制（此前为死常量未实施）；`limits.rs` 注释修正"均可在启动参数覆盖"的不实表述（O4） |
| N2 版本保留默认值 | ⏸ 跳过 | 把 `--version-retention` 默认 0 改为 >0 会在升级后静默删除历史版本（回滚/审计不可达）——有行为影响，仅文档提示 |
| O1 build-env.sh 硬编码路径 | ✅ 已修复 | 改为自动探测：CI 的 /home 布局存在则沿用；`~/.cargo` 已存在但不可写则回退工作区 `.cargo-local`；普通机器不覆盖 |
| O2 Dockerfile 以 root 运行 | ⏸ 跳过 | 改为非 root 会破坏 `deploy/docker-compose.yml` 命名卷（/data 由 root 创建）的写入，且本机无法构建验证——有行为影响，建议后续随 compose 改造一起做 |
| O3 compose 内置开发主密钥 | ✅ 已修复 | `DSH_MASTER_KEY` 改为 `${DSH_MASTER_KEY:-默认值}`（生产可注入；默认值仅限本机测试） |
| O4 文档/实现漂移 | ✅ 已修复 | `limits.rs` 注释修正；MAX_PROJECTS 生效（见 N2）；`VersionPruned` 错误码因无法区分"已裁剪/从未存在"（无墓碑）暂不产生，保留定义供未来快照元数据扩展 |

**第 3 轮验证**：`cargo fmt --check` PASS、`cargo clippy --all-targets --all-features -D warnings` 0 告警、
`cargo test --workspace` **121 用例全绿**；`/metrics` 冒烟——`dsh_session_active` 不再输出，`dsh_projects/dsh_master_key_ok/dsh_raft_role` 正常。

### 第 4 轮（2025-08-16 续）：deep-analysis F1–F20 全量实施（dev_docs/fix-design-p0-p1.md）

> 依据 dev_docs/deep-analysis-2025.md 的 F1–F20 + dev_docs/fix-design-p0-p1.md 设计；
> 实测：`cargo test --workspace` **130 用例全绿**、clippy/fmt 零告警、dev-single/api-surface/
> cluster/chaos 四个 e2e 脚本全过、集群轮换（共享主密钥）跨节点验证通过。

| 项 | 状态 | 实施要点 |
|----|------|----------|
| F1 HTTP watch 密文泄漏 | ✅ | dsh-core 新增 `wire.rs`（`masked_value`/`mask_event_for_wire`）；`watch_sse` 序列化前统一掩码（重放+实时唯一出口）；实测 watch 事件输出 `{"type":"string","str_value":"***"}` 无密文 |
| F2 branch_diff 密文泄漏 | ✅ | diff 值经 `masked_value` 输出（lib.rs branch_diff） |
| F3 join/raft token 默认关闭 | ✅ | 集群模式（--node-id）启动强制 `--join-token` 与 `--raft-token`（缺失报错退出）；脚本/README/compose 全部注入 demo token；实测无 token 启动报错 |
| F6 集群写响应 changes 恒空 | ✅ | `TypeConfig::R: u64 → WriteAck{version, events}`；apply 事件随响应返回；PublishService 移除多余 fallback；实测集群 publish 响应含 changes、结构发布 affected_branches 非空 |
| 部署 Dockerfile 缺 protoc | ✅ | builder 阶段补 `protobuf-compiler`；compose 密码/令牌改环境变量 |
| F9 secret 共享项明文 | ✅ | API+状态机双层校验：secret 项必须 type=secret 且值为字符串；type=secret 必须 secret=true |
| F7a redb 文件 0644 | ✅ | open 后 `chmod 0600`（含存量修复）+ unix 权限断言测试 |
| F7b KEK 明文进 Raft 日志 | ✅ | `RotateMasterKey.kek_enc`（当前 KEK 自加密新 KEK）；钩子逐 KEK 尝试解开；实测日志 `"kek":[]`、跨节点轮换后旧数据可解、ring 2 项 |
| F4 登录节流键伪造 | ✅ | `--trusted-proxy` CIDR 配置；未配置时用对端 socket 地址（PeerAddr 提取器），忽略 XFF；login/pa_login/rotate 转发透传 XFF；单测 3 个 |
| F8 转发无超时 | ✅ | `forward_request` 统一 helper（connect 3s + total 10s + XFF 透传），三处转发样板收敛 |
| SDK Go ctx/5s 掐断 | ✅ | gRPC 全部方法改用调用方 ctx（Watch 可取消）；SSE 用无整体超时独立 client；事件按版本去重；未知类型显式 nil |
| SDK TS 超时/声明/去重 | ✅ | request 加 AbortController 10s；动态 import 捕获回落 HTTP；声明改"gRPC Node-only"；watch 去重；listMembers 返回 Member[] |
| SDK Python 退避/去重 | ✅ | gRPC watch 指数退避（200ms→15s）；事件去重；空端点列表抛 ConfigError；tls 参数标注 no-op |
| UI 草稿编辑器数据破坏 | ✅ | 按类型渲染（bool 判 bool_value、number/textarea/password 分型回显与回读、NaN 显式报错）；secret 留空不改；分支选择保持；401 跳登录 |
| F5 SSE 慢消费者静默丢事件 | ✅ | `take_while(is_ok)`——Lagged/Closed 结束流（不再静默丢） |
| D-PRUNED 裁剪起点静默丢事件 | ✅ | gRPC 与 HTTP 重放前检测起点被裁剪 → 发 `snapshot_required` 事件并关流（SSE 合成事件带该标志） |
| D-TYPE 重放事件类型失真 | ✅ | `VersionRecord.event_ty` 落标（structure_publish/shared_cascade/rollback/value_publish）；重放直接用，旧日志回退推断 |
| F14 join 校验 | ✅ | node_id 未占用 + http/raft 地址 host:port 校验 |
| D-DEL deletes 静默丢弃 | ✅ | 无 `/` 的 deletes 条目返回 422 |
| D-DOC 契约文档矛盾 | ✅ | openapi snapshot 描述改"恒脱敏"；proto SECRET 注释改"数据面恒脱敏"；README 集群示例补 token |

**未实施（明确跳过/延后）**：D-CSP（内联脚本外置，与 UI 改造合批，P3）；D-STATUS（LeaderRedirect 409 语义，影响 SDK，P3）；D-N2（版本保留默认 0，有数据删除风险）；D-OPENAPI 补 PA 路由（文档项，P3）；`deploy/docker-compose.yml` root 用户（O2，需 compose 改造，P3）。

### 第 5 轮（2025-08-16 续）：P3 全项实施（deep-analysis §7 剩余项）

> 实测：`cargo test --workspace` **132 用例全绿**（+2 D-TEST）、clippy/fmt 零告警、4 个 e2e 全过、
> SDK gRPC/HTTP 契约测试全过（含新断言）、D2/D-STATUS 运行时实测、Admin UI 浏览器全流程实测。

| 项 | 状态 | 实施要点 |
|----|------|----------|
| D-CSP（F12） | ✅ | Admin 内联脚本外置为 `admin/app.js`（rust-embed 多文件 + content-type 按扩展名）；全部 onclick/onchange/onkeydown 改 `data-act` 事件委托（g/k 经 dataset 传递，无 JS 字符串注入面）；CSP `script-src 'self'` 移除 unsafe-inline；浏览器实测登录/建项目/切 tab 零 JS 错误 |
| D2 HTTP 数据面 token | ✅ | ApiState 增 `data_plane_token`；auth_middleware 对 /v1/* 校验 Bearer 或 `?token=`（兼容 SSE EventSource）；复用 `--data-plane-token`；SDK 对齐（Go authHeaders、TS watchHttp headers、Python _watch_http headers）；实测 401/200/200/401 全对 |
| D-STATUS（F11） | ✅ | LeaderRedirect 状态码 409→**428**（Precondition Required）；body 不变（ERR_LEADER_REDIRECT + leader_hint）；实测非 leader 写返回 428 |
| F10 openapi | ✅ | 补 `/api/v1/projects/{p}/admins`（GET/POST）与 `/admins/{u}`（PUT/DELETE）+ ProjectAdminAccount/PAUpsertRequest schema；check-contracts 39 paths OK |
| F13 expect 收敛 | ✅ | 29 处 `expect("sm lock")` 全收敛：lib.rs handler 统一 `lock_err` helper（500 不 panic）、watch_branch 锁中毒取内部值、publish.rs map_err、observability 告警/into_inner |
| D-TEST | ✅ | 三语言 grpc-test ListMembers 真断言（dev-single → FailedPrecondition）；契约脚本加 after_version 重放断言 + 事件字段断言（ty/request_id/structure_version）；dsh-watch 新增慢消费者（Lagged→流结束）与 force_snapshot（snapshot_required→结束）Rust 测试；force_snapshot 语义修正：补发后结束流（不接 live） |
| UI 功能面 | ✅ | reveal 入口（viewConfig + 明文审计开关）、集群管理 tab（members/promote/remove，PA 403 提示、dev-single 404 提示）、EventSource 带 after_version 续传 + 事件面板截断 200 条 |
| O2 compose 非 root | ✅ | Dockerfile 加 dsh 用户 + su-exec；`docker-entrypoint.sh` root 启动 chown /data 后降权 dsh；compose 无需改 |
