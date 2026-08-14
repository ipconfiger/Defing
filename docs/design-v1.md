# 分布式配置文档服务 —— 详细设计方案 v1.0

> 依据文档：[docs/proposal-v4.md](./proposal-v4.md)（需求规格 v4.1）、[docs/feasibility-report.md](./feasibility-report.md)（可行性分析 v4.1）
> 版本：v1.0 ｜ 状态：评审稿（供 M0 定稿与编码）
> 阅读对象：后端（Rust）、SDK（TS/Go/Python）、前端（Admin UI）工程师
> 本文约定：默认值即推荐值；标注"待定"的为开放决策点（见 §16）

---

## 0. 术语表

| 术语 | 含义 |
|------|------|
| 项目 Project | 顶级作用域（如 order-service） |
| 分支 Branch | 环境维度，默认 dev/test/prod + 自定义 |
| 分组 Group / item | 分支内配置组织；结构（分组+item 集合）在项目级定义 |
| 结构 Structure | 项目级唯一事实源：分组集合 + 每组的 item 定义（key/类型/校验/secret） |
| 草稿 Draft | 未发布修改：分支值草稿 / 项目结构草稿 / 共享库草稿 |
| 版本 Version | 不可变快照（结构 + 该分支值），每 (项目, 分支) 一条递增链 |
| 活动版本 ActiveVersion | 当前客户端可见的已发布版本（版本链最新） |
| 发布 Publish | 原子操作：固化草稿 → 新版本 → 推进指针 → 生成 diff → 通知 SDK |
| 回滚 Rollback | 基于历史版本内容创建新版本（历史不可变） |
| 共享库 SharedLibrary | 集群级共享项（分组+item），项目分组可引用 |
| 单一管理员会话 | 同一时刻仅一个有效 admin 登录（R10/AC14.10） |
| revision | Raft 状态机内部单调序号（日志序）；对外暴露版本号 |

## 1. 设计总览

### 1.1 系统定位
单二进制分布式配置服务：**Rust 主服务（Raft 集群 + 状态机 + API + 内嵌 Admin UI）＋ 三语言 SDK**。
配置按 项目→分支→分组→item 组织；修改走"草稿 → 版本 → 发布 → 通知"闭环。

### 1.2 核心不变量（实现必须保证，写进测试）

| # | 不变量 | 保证机制 |
|---|--------|----------|
| I1 | 写请求线性一致：返回成功即已复制到多数派并持久化 | Raft 单领导 + 多数派提交 |
| I2 | 防脑裂：任意时刻至多一个 leader 接受写 | Raft 选举安全（任期 + 日志匹配） |
| I3 | 结构恒等：项目下所有分支分组/item 完全一致 | 结构项目级单点定义，结构草稿发布全分支生效 |
| I4 | 草稿隔离：未发布修改对 SDK 不可见 | SDK 只读活动版本；草稿只存在于状态机 |
| I5 | 发布原子性：一次 Raft 写入完成 固化+版本+指针+diff+事件 | 发布引擎单 proposal |
| I6 | 版本不可变：已发布版本内容永不被修改 | 版本存快照/diff，指针只前移 |
| I7 | 单一管理员会话：同一时刻仅一个有效 admin 登录 | 会话记录在状态机内强制 |
| I8 | 敏感项静态加密：secret 值任何持久化形态均为密文 | 加密层在写入前/读取后 |
| I9 | 回滚可审计：回滚=创建新版本，不删除历史 | 版本链只增 |

### 1.3 总体架构

```
┌─────────────────────────────────────────────────────────┐
│ Rust 单二进制（每节点）                                    │
│  ┌───────────────────────────────────────────────────┐  │
│  │ API 层                                             │  │
│  │  ├ gRPC :8383  ── 数据面（SDK：Get/Watch/Members） │  │
│  │  ├ HTTP :8384  ── 管理面 + 渲染 + 健康检查          │  │
│  │  └ /admin      ── 内嵌 Admin UI（静态资源，同源）    │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 应用层                                              │  │
│  │  发布引擎 / 分支服务(diff,promotion) / 渲染引擎       │  │
│  │  / 加密层(AEAD) / 审计 / 会话(单管理员) / 共享库级联   │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 状态机（确定性应用）                                  │  │
│  │  项目/分支/分组/item + 草稿 + 版本链 + 共享库 + 会话   │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 共识层 openraft（日志/快照/成员变更）                 │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 存储 RocksDB（日志、快照、状态机 KV，内嵌）            │  │
│  └───────────────────────────────────────────────────┘  │
│  节点间：Raft 端口 :8385（成员复制）                      │
└─────────────────────────────────────────────────────────┘
```

### 1.4 模块清单

| 模块 | 职责 | 关键依赖 |
|------|------|----------|
| raft-node | openraft 集成：日志、快照、成员变更、选举 | openraft, rocksdb |
| core | 状态机与数据模型：项目/分支/结构/草稿/版本/共享库 | serde |
| publish | 发布引擎：固化、diff、事件、回滚、级联 | core |
| api | gRPC + HTTP 接口、错误码、鉴权 | tonic, axum |
| watch | 订阅管理与事件扇出、断线重放 | tokio |
| crypto | 加密层：AEAD、信封、轮换、脱敏 | aes-gcm/ring |
| render | 规范化 IR → YAML/TOML/JSON，引用解析 | serde_yaml, toml, serde_json |
| admin-ui | 前端产物内嵌与静态托管 | rust-embed, axum |
| observability | healthz/readyz、指标、结构化日志、审计 | tower-http, tracing |

## 2. 集群与共识层

### 2.1 节点角色与生命周期
- 角色：bootstrap 首节点 / 普通成员 / learner（追赶中）→ voter。
- 状态：follower → candidate → leader（Raft 标准）。

### 2.2 加入协议（join）
1. 新节点启动：`--join http://host:8384`（任意已有实例的管理面端点）。
2. 新节点调用 `JoinCluster{join_addr, node_id}`（写请求，需经 leader）。
3. leader 校验后：将新节点加入成员表为 **learner**，返回集群信息（leader 地址、其他成员）。
4. learner 从 leader 拉取快照 + 日志追赶（限速，默认 64MB/s 可配）。
5. 追平后，管理员（或配置 `--auto-promote`）将 learner 提升为 voter。
6. 新节点成为 voter 后开始服务读写。

```
新节点 ──join──▶ 任意节点 ──Raft写──▶ leader 加 learner
   ◀──集群信息──    │
   ──快照+日志追赶──▶ leader（限速）
   ◀──追平确认──    │
   ──promote──▶ voter（此后参与投票与服务）
```

### 2.3 快照与日志
- openraft 快照：由状态机定期生成（默认每 10k 条日志或 64MB，可配），存 RocksDB。
- 新节点/落后节点从 leader 拉快照 → 应用 → 追日志 → 追平。
- 日志压缩：快照之后截断旧日志（保留最近 N 条用于追赶，可配）。

### 2.4 选举与故障
- 心跳默认 500ms，选举超时 1.5~3s 随机（可配）；多数派存活才可选出 leader。
- 少数派存活：拒绝写（I1/I2），读可选（follower 读已提交数据）。

### 2.5 读路径与 leader 重定向
- **线性一致读**：默认走 leader（或 ReadIndex 校验后本地读），保证读已提交。
- **follower 读**：可配置（`--read-mode=stale`），返回可能稍旧的已提交数据。
- 写请求到达非 leader：返回 `ERR_LEADER_REDIRECT` + leader 地址（SDK 自动跟随）。

### 2.6 启动参数（极简配置 R12）
| 参数 | 说明 | 默认 |
|------|------|------|
| `--bootstrap` | 首节点自举 | 无（二选一必填） |
| `--join <endpoint>` | 加入集群（指定任一实例 HTTP 端点） | 无 |
| `--data-dir` | 数据目录 | `./data` |
| `--grpc-addr` | 数据面 gRPC | `:8383` |
| `--http-addr` | 管理面 HTTP | `:8384` |
| `--raft-addr` | 节点间 Raft | `:8385` |
| `--advertise-addr` | 对外公告地址（NAT/容器场景） | 自动推断 |
| `--read-mode` | linear | stale | linear |
| 环境变量 | 全部参数可用 `DSH_*` 环境变量覆盖 | 优先级：env > file > flag |

## 3. 数据模型（详细）

### 3.1 逻辑模型（ER 文字版）

```
Project { id, name, created_at }
  ├─ Structure { project_id, version, groups[] }        // 已发布结构（不可变，版本递增）
  │     groups[] = { name, items[] }
  │       items[] = { key, type, required, secret, validate }
  ├─ StructureDraft { project_id, base_version, groups[] } // 未发布结构修改
  ├─ Branch { project_id, name, created_at }
  │     └─ BranchState { project_id, branch,
  │          active_version,                 // 活动版本号
  │          value_draft,                    // (group,key) → 草稿值（未发布）
  │          versions: Version[] }           // 版本链（不可变）
  │            Version { no, structure_version, created_at, operator, comment,
  │                      snapshot_ref, diff }
  └─ RefBinding { project_id, group, item_key? , shared_group, shared_key }  // 引用绑定

SharedLibrary
  ├─ SharedDraft { group, key, ... }          // 未发布共享修改
  ├─ SharedItem { group, key, type, secret, required, validate, value, version }
  └─ (共享版本链：可选保留，MVP 保留最近 N 个共享版本)

AdminSession { token_hash, issued_at, expires_at, device_id }   // 全局唯一活动会话
AuditLog { id, ts, operator, action, target, detail, version? }
```

### 3.2 存储模型（状态机 KV 前缀布局，RocksDB）

| Key 前缀 | 内容 |
|----------|------|
| `p/{pid}` | 项目元数据 |
| `p/{pid}/struct` | 已发布结构（含结构版本号） |
| `p/{pid}/struct-draft` | 结构草稿 |
| `p/{pid}/b/{branch}/state` | 分支状态（active_version、value_draft） |
| `p/{pid}/b/{branch}/v/{no}` | 版本（快照或 diff 引用） |
| `p/{pid}/refs/{g}/{k?}` | 共享库引用绑定 |
| `sh/{g}/{k}` | 共享项（含版本） |
| `sh-draft/{g}/{k}` | 共享草稿 |
| `sess/admin` | 活动会话记录（单会话强制） |
| `audit/{seq}` | 审计日志 |
| `meta/cluster` | 集群元数据（成员等，openraft 管理） |

### 3.3 值类型与校验

| 类型 | 校验 | 存储 |
|------|------|------|
| string | 长度/正则（validate 可配） | 明文 |
| int / float | 范围校验 | 明文 |
| bool | — | 明文 |
| json | 合法 JSON（存规范化文本） | 明文 |
| array | 元素类型约束 | 明文 |
| **secret** | 强制加密（I8） | 密文（信封加密，§7） |

item 定义：`{ key, type, required(bool), secret(bool), validate? }`；
值可为空（未填）；**发布时校验 required**（默认阻断，可配警告，见 §4.6）。

### 3.4 结构强一致与结构草稿
- 结构（分组+item 定义）只存在项目级：`p/{pid}/struct` 一份。
- 结构修改 → `p/{pid}/struct-draft`；**发布结构** = 一次 Raft 写入：新结构版本 + 全部分支
  各推进一个版本（值不变、结构版本号同步）→ 所有分支结构恒等（I3）。
- 新建分支：继承当前已发布结构 + 活动版本值（从活动版本物化出初始值草稿）。

### 3.5 版本链：快照 + diff（checkpoint）
- **活动版本**：存全量快照（读快）。
- **历史版本**：默认存 diff（相对前一版本）；每 N 个版本（默认 100）生成一个 checkpoint 全量快照。
- 读取历史版本：从最近 checkpoint 重建（应用 diff）。
- **保留策略**：默认全量保留；可配 `--version-retention-count` / `--version-retention-days`；
  裁剪只删历史、不动活动版本与 checkpoint 保底（至少保留最近 1 个 checkpoint 之后的历史）。
- **版本物化（materialization）**：发布时把共享库引用解析后的**值写入版本快照**，
  版本自包含、不可变（共享库后续变更不影响历史版本）。

## 4. 版本与发布引擎

### 4.1 状态机（分支值发布）

```
编辑(API/UI) ──▶ 值草稿 value_draft
                       │
                Publish{comment}（一次 Raft 写入）
                       ├─ 1. 校验：权限、required、validate、引用解析
                       ├─ 2. 快照草稿 → 解析共享引用 → 物化
                       ├─ 3. 生成 diff（vs 活动版本）
                       ├─ 4. 创建版本 no+1（快照/diff 写入）
                       ├─ 5. 推进 active_version
                       ├─ 6. 清空已发布草稿（记录"已消费"）
                       └─ 7. 事件入队 → 异步扇出给订阅 SDK
```

### 4.2 发布原子性（I5）
- 以上 1–6 全部在**单个 Raft proposal 的处理函数内**完成：要么整体生效，要么整体失败。
- 事件扇出（第 7 步）在提交后异步执行（不阻塞写入路径）。
- 版本号：`(项目, 分支)` 内单调递增 int64，从 1 开始，永不复用。

### 4.3 结构发布
- 触发：`PublishStructure{comment}`；写入：新结构版本 + 每个分支各推进一个版本
  （diff 为结构变化，值不变）→ 一次 Raft 写入完成，原子。
- 发布前在 UI 展示影响预览：波及分支列表、受影响 item 列表。

### 4.4 回滚
- `Rollback{project, branch, to_version, comment}`：
  1. 读取 to_version 的快照（checkpoint 重建，见 §3.5）
  2. 以该内容创建**新版本** no+1（operator/comment/rollback_of=to_version 入审计）
  3. 推进活动版本，事件照常推送（SDK 无需特殊处理）
- 历史版本永不被修改（I6/I9）；"再回滚"就是又一次 Rollback。

### 4.5 并发控制（v4.1 决策：单一管理员会话）
- **人工并发**：单会话强制（I7）——第二个登录被拒（`ERR_SESSION_IN_USE`），会话状态在
  状态机内（`sess/admin`），集群范围线性一致地保证唯一。
- **程序化并发**（CI 令牌发布）：发布本身是单 Raft 写入 + 快照语义，天然串行；
  企业版可加"发布锁"实现绝对串行。
- 发布与编辑竞争：编辑在发布后继续进入下一版草稿；UI 提示"发布时刻的草稿内容"。

### 4.6 发布前完整性校验
- 校验项：required 未填、类型/规则不符、引用解析失败、循环引用（共享库）。
- 策略：默认**阻断发布**（返回 `ERR_PUBLISH_BLOCKED` + 明细）；`--publish-policy=warn` 可降级为警告并发布。

## 5. API 与协议设计

### 5.1 传输
- 数据面：gRPC（protobuf，含 Watch 服务端流）；错误经 gRPC status + 自定义错误码。
- 管理面：REST/JSON（axum）；渲染与健康检查也走 HTTP。
- Admin UI：同源 `/admin`（免 CORS）。

### 5.2 数据面 API（SDK 用，proto 伪代码）

```proto
service Config {
  rpc GetConfig(GetConfigRequest) returns (ConfigSnapshot);
  rpc GetItem(GetItemRequest) returns (ItemValue);
  rpc Watch(WatchRequest) returns (stream WatchEvent);   // 服务端流
  rpc ListMembers(ListMembersRequest) returns (ListMembersResponse);
}
message GetConfigRequest { string project; string branch; int64 version = 0; } // version=0 取活动版本
message ConfigSnapshot  { int64 version; int64 structure_version; map<string, GroupData> groups; }
message WatchRequest    { string project; string branch; int64 after_version = 0; }
message WatchEvent      { int64 version; EventType type; string comment;
                          repeated Change changes; }       // Change{group, key, new_value}
enum EventType { VALUE_PUBLISH=0; STRUCTURE_PUBLISH=1; SHARED_CASCADE=2; ROLLBACK=3; }
```

- `after_version>0`：重放该版本之后的所有已发布事件（断线续传，§6）。

### 5.3 管理面 API（REST，Admin/CLI 用，节选）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | /api/login | 登录（单会话强制） |
| POST | /api/logout / /api/heartbeat | 登出 / 心跳续期 |
| CRUD | /api/projects, /api/projects/{p}/branches | 项目/分支（分支新建继承结构） |
| CRUD | /api/projects/{p}/structure-draft | 结构草稿（含影响预览） |
| POST | /api/projects/{p}/structure-draft/publish | 发布结构（全分支生效） |
| CRUD | /api/projects/{p}/branches/{b}/draft | 值草稿编辑 |
| POST | /api/projects/{p}/branches/{b}/publish | 发布版本（body: {comment}） |
| GET | /api/projects/{p}/branches/{b}/versions | 版本历史 |
| POST | /api/projects/{p}/branches/{b}/rollback | 回滚（body: {to_version, comment}） |
| GET | /api/projects/{p}/diff?branch_a&branch_b | 分支对比 |
| POST | /api/projects/{p}/promote | 值提升（body: {from, to, items?}，写入目标分支草稿） |
| CRUD | /api/shared, /api/shared-draft, POST /api/shared/publish | 共享库（发布即级联） |
| GET | /api/audit | 审计查询 |
| GET | /healthz /readyz | 健康检查 |
| GET | /v1/projects/{p}/branches/{b}/config?format=&version= | 渲染输出（YAML/TOML/JSON） |

### 5.4 错误码

| 错误码 | 含义 | 客户端动作 |
|--------|------|-----------|
| ERR_LEADER_REDIRECT | 非 leader | SDK 跟随 leader_hint |
| ERR_NOT_FOUND | 不存在 | — |
| ERR_VALIDATION | 校验失败（带明细） | 提示用户 |
| ERR_PUBLISH_BLOCKED | 完整性校验阻断 | 展示明细 |
| ERR_VERSION_PRUNED | 续传起点已被裁剪 | 重新拉全量快照 |
| ERR_SESSION_IN_USE | 已有管理员在线 | 提示等待/强制下线 |
| ERR_SESSION_EXPIRED | 会话过期 | 重新登录 |
| ERR_FORBIDDEN | 无权限 | — |
| ERR_CYCLE_REF | 共享引用成环 | 提示 |
| ERR_CONFLICT | 乐观锁冲突 | 刷新重试 |

### 5.5 协议版本化
- proto 包名带 major（`config.v1`）；破坏性变更升 major；SDK 声明支持的最低版本。
- 管理面 API 前缀 `/api/v1`。

## 6. Watch 与推送

### 6.1 事件模型
- 事件只由**发布**产生（I4）：`{version, type, comment, changes[]}`。
- changes：变更 item 的 {group, key, new_value}（结构发布为新增/删除的 item 定义；级联为引用共享项的 item）。

### 6.2 订阅生命周期
1. `Watch{project, branch, after_version=0}`：服务端先回当前版本号（+可选全量，SDK 用 GetConfig）。
2. 服务端持续推送后续发布事件。
3. 断线：SDK 记录 last_version；重连 `after_version=last_version`。
4. 服务端重放 last_version 之后的版本事件（从版本链/事件日志），再转实时流。
5. 若起点已被裁剪（`ERR_VERSION_PRUNED`）：SDK 拉全量快照重置。

### 6.3 服务端实现
- 每 (项目, 分支) 订阅表：`HashMap<(pid, branch), Vec<WatchStream>>`（每节点本地；事件在
  leader 提交后广播到所有节点，各节点扇出给本地订阅者——容错 follower 上的订阅）。
- 事件日志保留：最近 N 个事件（默认 10k）供重放；重放优先用版本链 diff。
- 限流：单订阅事件 QPS 上限、订阅数上限（默认 10k/节点，可配）。

### 6.4 推送通道
- 首选 gRPC 服务端流（低延迟、类型化、TLS）。
- 备选长轮询（代理穿透场景）；TS 补充 SSE。三通道语义一致（同事件模型）。

## 7. 加密设计

### 7.1 威胁模型（覆盖范围）
- 静态威胁：磁盘/备份/快照被读 → 密文。
- 传输威胁：TLS 保护（gRPC/HTTP 均支持，默认提示开启）。
- 不在范围：内存侧信道、恶意管理员（可解密）、密钥管理系统（KMS 为企业版）。
- **明文仅存在于解密瞬间**（I8）。

### 7.2 算法与密钥层次
- 数据加密：AEAD **AES-256-GCM**（首选，硬件加速）或 **ChaCha20-Poly1305**（软件快）。
- 层次（信封加密）：
  ```
  主密钥 KEK（来自 env/file/KMS，仅内存）
    └─ 每 item 随机数据密钥 DEK（AES-256，随密文存储）
         └─ 数据密文 = AEAD_enc(DEK, nonce, 明文)
       附：encrypted_DEK = AEAD_enc(KEK, nonce2, DEK)（存于 item 元数据）
  ```

### 7.3 写/读流程
- 写（草稿/发布/共享库）：secret 值在**进入 Raft 提案前**加密（密文进状态机）。
- 读（SDK Get/渲染）：状态机内为密文，解密发生在出站前瞬间；管理面脱敏展示（`***`）。
- 版本快照/diff 中同样只存密文。

### 7.4 主密钥来源
- 抽象 `KeyProvider` trait：env（`DSH_MASTER_KEY`）/ 密钥文件（`--master-key-file`）/ KMS（企业版插件）。
- 主密钥**不明文落盘**；密钥文件权限建议 0400；提供 `--gen-master-key` 生成指引。

### 7.5 轮换与版本化
- DEK 版本化：item 元数据含 `dek_version`；轮换 = 用新 KEK 重加密所有 DEK（不重加密数据）。
- 主密钥轮换：`dsh admin rotate-master-key` → 后台任务重包 DEK；轮换期间新写用新 KEK，旧数据可解（保留旧 KEK 列表直到全部重包完成）。

### 7.6 脱敏与审计
- 管理界面/导出：secret 默认掩码（`redis://***@host:6379`）；`?reveal=true` 需二次确认并审计。
- 解密/导出/回滚含 secret 的版本 → 审计日志（操作者、时间、目标、版本）。

## 8. 多格式渲染

### 8.1 规范化中间表示（IR）
- IR = 活动版本（或指定版本）物化后的 `map<group, map<key, value>>`（共享引用已解析）。
- 渲染前统一校验：类型、TOML 表达力（见 8.2）。

### 8.2 渲染约束
- **以 TOML 为表达力下限**：键限合法标识符或引号串；顶层必须是表（分组→顶层表）；
  json 类型值在 TOML 中输出为内联表/数组或字符串（规则文档化）；无 null（secret 未填 → 输出占位注释）。
- YAML：serde_yaml（1.x 稳定）；JSON：serde_json；TOML：toml crate。
- 三格式**语义等价**：由等价性测试保证（§14）。

### 8.3 引用解析与渲染期校验
- 渲染/发布时解析 RefBinding → 共享项值；解析失败 → 阻断（ERR_VALIDATION 明细）。
- 循环引用检测：构建引用图，DFS 判环，发布与渲染前执行。

### 8.4 等价性校验
- 属性测试：随机 IR → 三格式 → 各自解析 → 语义等价（规范化比较）。

## 9. Admin UI

### 9.1 技术选型
- 前端：React + TypeScript + Vite（产物 ≤5MB 基准）；构建后由 `rust-embed` 编译进二进制。
- 托管：axum `/admin` 静态 + SPA fallback；与 API 同源（免 CORS）；CSP 头、无外链。

### 9.2 页面结构
- 登录页（单会话）、项目列表、项目详情（分支 Tab）、分支编辑页
  （树形：分组→item 值编辑器 + **待发布变更视图** + **发布按钮**（校验结果 + 影响预览））、
  版本历史页（版本列表 + 版本 diff + **回滚按钮**）、分支对比页（diff + **promote**）、
  共享库页（CRUD + 引用绑定 + 级联预览）、审计页、设置页（主密钥/保留策略/成员）。

### 9.3 单一管理员会话（I7）
- 登录成功 → 签发会话（token 存内存；状态机记 `sess/admin`：token_hash、expires_at、device_id）。
- 第二个登录 → `ERR_SESSION_IN_USE`：UI 提示"已有管理员在线（设备/时间）"，提供"强制下线"按钮
  （需再次确认，写状态机替换会话）。
- 心跳：前端每 5min 调 `/api/heartbeat` 续期；TTL 默认 24h（可配）；服务端过期即清除。
- CLI 兜底：`dsh admin force-logout`（管理员忘退时恢复）。

### 9.4 权限分离
- 数据面 API（SDK）与管理面 API 鉴权独立：SDK 用 API 令牌（只读/watch）；管理面用会话（读写）。
- MVP 无 RBAC；企业版：分支级权限、SSO、MFA。

### 9.5 构建与内嵌
- CI：前端构建 → 产物哈希固定 → 内嵌 → 单元测试含静态资源存在性检查。
- 体积基准：前端 ≤5MB；总二进制 ≤50MB。

## 10. SDK 契约（TS / Go / Python）

### 10.1 共同行为（三语言一致，契约测试覆盖）
- **端点池与 failover**：`ConfigClient(endpoints: string[])`；按序/权重连接；失败自动切换 +
  指数退避（base 500ms，cap 30s，抖动）；`ERR_LEADER_REDIRECT` 自动跟随 leader_hint。
- **订阅**：`watch(project, branch, listener)`；首次连接后缓存活动版本（`GetConfig`）；
  收到 `WatchEvent` → 增量更新本地缓存 → 回调 listener（事件含 version，保证顺序）。
- **断线续传**：重连携带 `after_version`；`ERR_VERSION_PRUNED` → 重拉全量。
- **API**：`get(project, branch) / getItem(project, branch, group, key) / watch(...) / close()`。
- **TLS**：支持 `ConfigClient(endpoints, {tls})`；令牌 `Authorization: Bearer`。

### 10.2 语言差异
- TS：浏览器（fetch + EventSource/WebSocket）与 Node（gRPC 流）双运行时；类型化 `WatchEvent<T>`。
- Go：gRPC client + goroutine 扇出 listener；`context.Context` 贯穿；`sync.Once` 关闭。
- Python：async（`asyncio` + grpc.aio）；listener 为 async 回调；线程安全本地缓存。

### 10.3 契约测试
- 三语言 SDK 对**同一 mock 服务**（Rust 测试桩，协议 golden）跑同一套用例：
  端点池切换、leader 重定向、watch 顺序、断线重放、VERSION_PRUNED 重置、secret 脱敏、多格式获取。

## 11. 安全设计

| 项 | 设计 |
|----|------|
| 传输 | gRPC/HTTP 均支持 TLS；默认启动时提示开启；生产建议强制 |
| 认证 | 管理面：会话（单会话强制）；数据面：API 令牌（只读/watch） |
| 鉴权 | MVP 全局管理员；企业版分支级 RBAC |
| 会话 | TTL + 心跳 + 强制下线 + 登录失败限次（防爆破）+ 设备绑定 |
| 密钥 | §7 信封加密；主密钥不明文落盘 |
| Web | CSP、XSS/CSRF 防护、初始凭证强制修改、无外链资源 |
| 审计 | 登录/登出、发布/回滚/结构发布/共享发布、解密与导出、promote、会话强制下线 |
| 供应链 | cargo deny + RustSec；前端 lockfile + SBOM；产物哈希固定 |

## 12. 可观测性

- 健康检查：`/healthz`（进程存活）、`/readyz`（已加入集群且可服务：leader 或 follower 可读）。
- Prometheus 指标（`/metrics`）：
  `raft_role / raft_leader / raft_term / raft_committed_index / raft_snapshot_size`、
  `api_qps / api_latency{grpc,http} / watch_conns / watch_events_total`、
  `publish_total / rollback_total / versions_total / drafts_pending / shared_refs_total`、
  `storage_bytes / session_active`。
- 结构化日志：JSON 可选；含 request_id、操作者、版本号；审计独立表（`audit/{seq}`）。
- 追踪：OpenTelemetry（企业版）。

## 13. 配置与部署

- 极简启动：`dsh --bootstrap`（首节点）/ `dsh --join http://host:8384`（加入）。
- 配置优先级：环境变量（`DSH_*`）> 配置文件（`--config dsh.yaml`）> 命令行。
- Docker：`dsh:latest` 镜像（静态编译，scratch/alpine）；`docker-compose.yml` 一键三节点
  （bootstrap + 2×join + 3 个数据卷）。
- 数据目录：`./data`（RocksDB + 快照 + 日志）；备份：快照 + 日志归档；恢复指引文档。

## 14. 测试与质量

| 层 | 内容 |
|----|------|
| 单元 | 数据模型、校验器、diff、加密（KAT 向量）、渲染器 |
| 集成 | API 全链路、发布引擎状态机、共享级联、回滚 |
| Raft 故障注入 | 分区（leader 隔离/少数派）、丢包、乱序、节点 kill/重启、快照追赶 |
| watch 测试 | 断线重放、VERSION_PRUNED、事件顺序、fan-out 压力 |
| 加密测试 | 轮换、DEK 版本化、旧 KEK 兼容、脱敏 |
| 等价性测试 | 随机 IR → YAML/TOML/JSON 语义等价（属性测试） |
| 契约测试 | 三 SDK vs mock 服务同一套用例 |
| 混沌 | 集群级混沌演练（配合故障注入） |
| 基准 | 写 QPS ≥10k、watch 连接 ≥10k、发布→SDK ≤1s、单机内存 ≤128MB、二进制 ≤50MB |

## 15. 里程碑与模块依赖

| 阶段 | 交付物 | 依赖 |
|------|--------|------|
| M0（2 周） | 协议规范定稿（proto/REST 契约）、数据模型文档、CI 脚手架、决策点关闭 | — |
| M1（4~6 周） | raft-node（bootstrap/join/快照）、core（CRUD + 草稿）、storage、极简配置 | openraft, rocksdb |
| M2（4~6 周） | publish 引擎（发布/结构发布/回滚/校验）、共享库+级联、diff/promote、crypto、render | core |
| M2.5（3~4 周） | admin-ui（树形/草稿/发布/历史/回滚/单会话）、watch 服务端、可观测性 | api |
| M3（4~6 周） | 三 SDK + failover + watch + 续传 + 契约测试 | api/watch |
| M4（持续） | 混沌、加固、文档、发布、compose 示例 | 全部 |

**MVP 收敛**：M1+M2 子集（不含共享库、多格式 TOML/YAML、promote 完整版）＋基础 Admin UI＋
watch（发布通知）＋secret 加密。共享库、多格式、三 SDK 完整版按里程碑推进。

## 16. 开放决策点（推荐默认已给出，M0 前关闭）

| # | 决策 | 推荐默认 | 备注 |
|---|------|----------|------|
| D1 | 回滚粒度 | **整版本回滚**（§4.4） | 单 item 回滚列后续 |
| D2 | 发布完整性校验默认 | **阻断**（`--publish-policy=block`） | 可配 warn |
| D3 | 版本存储 | **快照 + diff（checkpoint，每 100 版）** | 见 §3.5 |
| D4 | 灰度发布 | 企业版第一期 | 不进开源 MVP |
| D5 | 会话 TTL/心跳 | TTL 24h / 心跳 5min / 空闲可配 | 见 §9.3 |
| D6 | 单会话语义 | **拒绝第二个登录**（可加"强制下线"按钮） | v4.1 已决策，细节可调 |
| D7 | 共享库级联 | 自动级联 + 显式发布开关（防风暴） | 见 §4 发布引擎 |
| D8 | 存储引擎 | RocksDB（rust-rocksdb） | sled 已停维护，不用 |
| D9 | 共识库 | **openraft**（备用 raft-rs） | 见 §2 |
| D10 | 分组嵌套 | v4 默认平铺一层 | 嵌套列后续 |

## 17. 附录

### 17.1 仓库结构建议
```
repo/
  server/            # Rust 工作区
    crates/core/     # 数据模型 + 状态机
    crates/raft-node/# openraft 集成
    crates/storage/  # rocksdb
    crates/publish/  # 发布引擎
    crates/api/      # tonic + axum
    crates/watch/    # 订阅与扇出
    crates/crypto/   # 加密层
    crates/render/   # 多格式渲染
    crates/admin-ui/ # 内嵌前端产物
    proto/           # 协议定义
  sdk/ts/  sdk/go/  sdk/python/
  deploy/            # Dockerfile + docker-compose.yml
  docs/              # 本文档与需求/可行性文档
```

### 17.2 核心不变量 → 测试映射（节选）
- I1/I2 → Raft 故障注入套件
- I3 → 结构一致性校验测试（任意操作后断言全分支结构恒等）
- I4 → SDK 契约测试：草稿编辑期间 GetConfig 不变
- I5 → 发布原子性测试：断点注入（模拟提案中崩溃），断言无半发布状态
- I6 → 版本不可变测试：发布后试图修改旧版本被拒
- I7 → 并发登录测试：第二个登录收到 ERR_SESSION_IN_USE
- I8 → 加密测试：磁盘文件/快照/备份中无明文
- I9 → 回滚审计测试

### 17.3 参考
- [docs/proposal-v4.md](./proposal-v4.md)、[docs/feasibility-report.md](./feasibility-report.md)
- [openraft](https://github.com/databendlabs/openraft)、[raft-rs](https://github.com/tikv/raft-rs)、[axum](https://github.com/tokio-rs/axum)、[tonic](https://github.com/hyperium/tonic)
- [rust-embed](https://docs.rs/rust-embed)、[aes-gcm](https://docs.rs/aes-gcm)、[etcd API guarantees](https://etcd.io/docs/v3.8/learning/api_guarantees/)
