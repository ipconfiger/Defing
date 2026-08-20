# 分布式配置文档服务 —— 详细设计方案 v2.0（审核修订版）

> 依据文档：[docs/proposal-v4.md](./proposal-v4.md)（v4.1）、[docs/feasibility-report.md](./feasibility-report.md)（v4.1）、
> 上一版：[docs/design-v1.md](./design-v1.md)（v1.0）
> 版本：v2.0 ｜ 状态：评审稿（M0 可直接依据本版定稿并编码）
> 本版要点：① 深度审核 v1（覆盖 §17 修订记录）；② §16 决策点全部闭合为默认+理由；
> ③ 契约转正式文件 [proto/config.v1.proto](../proto/config.v1.proto)；④ 补齐实现级细节
> （事务伪代码、级联算法、幂等、限额、慢消费者、初始凭证、备份恢复等）

---

## 0. 术语表

| 术语 | 含义 |
|------|------|
| 项目/分支/分组/item | 四级组织模型（见 §3） |
| 结构 Structure | 项目级唯一事实源：分组+item 定义 |
| 草稿 Draft | 未发布修改（值草稿/结构草稿/共享草稿） |
| 版本 Version | 不可变快照；(项目, 分支) 内单调递增版本链 |
| 活动版本 | 当前客户端可见版本（版本链最新） |
| 发布 Publish | 原子操作：固化草稿→新版本→推进指针→diff→通知 |
| 回滚 Rollback | 基于历史版本内容创建新版本（历史不可变） |
| 单一管理员会话 | 同一时刻仅一个有效 admin 登录（I7） |
| revision | Raft 内部日志序（不对外暴露）；对外统一用版本号 |
| client_request_id | 管理写操作的幂等键（§4.2/D11） |
| 物化 materialization | 发布时把共享引用解析后的值写入版本快照（版本自包含） |

## 1. 设计总览

### 1.1 系统定位
单二进制分布式配置服务：**Rust 主服务（openraft 集群 + 确定性状态机 + gRPC/HTTP API + 内嵌 Admin UI）＋
TS/Go/Python 三语言 SDK**。配置按 项目→分支→分组→item 组织；修改走"草稿→版本→发布→通知"闭环。

### 1.2 核心不变量（实现必须保证，映射测试见 §18.2）

| # | 不变量 | 保证机制 |
|---|--------|----------|
| I1 | 写线性一致：成功即已复制到多数派并持久化 | Raft 单领导 + 多数派提交 |
| I2 | 防脑裂：任意时刻至多一个 leader 接受写 | Raft 选举安全 |
| I3 | 结构恒等：项目下所有分支分组/item 完全一致 | 结构项目级单点定义 |
| I4 | 草稿隔离：未发布修改对 SDK 不可见 | SDK 只读活动版本 |
| I5 | 发布原子性：一次 Raft 写入完成全部步骤 | 发布引擎单 proposal |
| I6 | 版本不可变：已发布版本内容永不被修改 | 版本只增、指针只前移 |
| I7 | 单一管理员会话 | 会话记录在状态机内强制 |
| I8 | 敏感项静态加密：secret 任何持久化形态均为密文 | 加密层在写入前 |
| I9 | 回滚可审计：回滚=创建新版本，不删历史 | 版本链只增 |
| I10 | 幂等：同 client_request_id 的发布/回滚不重复生效 | 状态机内 last_request_id（§4.2） |

### 1.3 总体架构

```
┌─────────────────────────────────────────────────────────┐
│ Rust 单二进制（每节点）                                    │
│  ┌───────────────────────────────────────────────────┐  │
│  │ API 层                                             │  │
│  │  ├ gRPC :8383 数据面（SDK：GetConfig/GetItem/Watch │  │
│  │  │             /ListMembers）—— 契约见 proto        │  │
│  │  ├ HTTP :8384 管理面（REST /api/v1）+ 渲染 + 健康检查│  │
│  │  └ /admin     内嵌 Admin UI（同源）                 │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 应用层                                              │  │
│  │  发布引擎(事务) / 共享级联 / diff·promote / 渲染引擎   │  │
│  │  / 加密层(AEAD·信封) / 会话(单管理员) / 审计           │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 状态机（确定性应用，openraft 日志驱动）               │  │
│  │  项目/分支/结构/草稿/版本链/共享库/引用/会话/审计       │  │
│  ├───────────────────────────────────────────────────┤  │
│  │ 存储 RocksDB（日志/快照/状态机 KV，内嵌）             │  │
│  └───────────────────────────────────────────────────┘  │
│  节点间：Raft :8385；后台任务：版本裁剪/密钥重包/审计清理   │
└─────────────────────────────────────────────────────────┘
```

### 1.4 模块清单（v2 新增后台任务模块）

| 模块 | 职责 | 关键依赖 |
|------|------|----------|
| raft-node | openraft 集成：日志、快照、成员变更、选举 | openraft, rocksdb |
| core | 状态机与数据模型 | serde |
| publish | 发布引擎：固化、diff、幂等、回滚、级联 | core |
| api | gRPC + HTTP、错误码、鉴权、幂等头 | tonic, axum |
| watch | 订阅管理、事件扇出、慢消费者、重放 | tokio |
| crypto | AEAD、信封、轮换、脱敏 | aes-gcm/ring |
| render | IR → YAML/TOML/JSON、引用解析 | serde_yaml, toml |
| admin-ui | 前端产物内嵌与静态托管 | rust-embed |
| observability | healthz/readyz、指标、日志、审计 | tower-http, tracing |
| jobs | 后台任务：版本裁剪、DEK 重包、审计清理 | tokio, core |

## 2. 集群与共识层

### 2.1 节点身份与生命周期（v2 补充）
- `--node-id`：显式指定；未指定则首启生成 UUID 并持久化到 data-dir（`identity.json`）。
- **重启/重连**：节点启动时若 identity 已存在且成员表仍含该节点 → 自动以原成员身份 rejoin
  （无需 `--join`）；否则视为新节点走 join 流程。身份与成员表不一致时提示手动修复。
- 角色：bootstrap 首节点 / 成员 / learner（追赶中）→ voter。

### 2.2 加入协议（join）
1. 新节点启动：`--join http://host:8384`（任意已有实例管理面端点）。
2. 调用 `POST /api/v1/cluster/join {node_id, grpc_addr, raft_addr}`（写请求，经 leader）。
3. leader 校验后加为 **learner**，返回集群信息（成员表、leader 地址）。
4. learner 从 leader 拉快照 + 日志（限速默认 64MB/s，`--snapshot-limit` 可配）。
5. 追平后：`POST /api/v1/cluster/promote {node_id}` 提升为 voter（或 `--auto-promote`）。
6. 移除节点：`POST /api/v1/cluster/remove {node_id}`（先降 learner 再移除，openraft 成员变更
   保证过程中无脑裂窗口）。

### 2.3 快照与日志
- 快照生成：默认每 10k 条日志或 64MB（`--snapshot-interval` / `--snapshot-size`）。
- 日志压缩：快照后截断（保留最近 `--log-retain` 条供追赶，默认 5k）。
- 落后节点：快照 → 应用 → 追日志 → 追平。

### 2.4 选举与故障
- 心跳 500ms；选举超时 1.5~3s 随机（`--election-timeout-min/max`）。
- 多数派存活才能选出 leader；少数派拒绝写（I1/I2），读受 `--read-mode` 控制（§2.5）。

### 2.5 读路径与 leader 重定向
- `--read-mode=linear`（默认）：读走 leader 或 ReadIndex 校验后本地读（读已提交）。
- `--read-mode=stale`：follower 本地读（可能稍旧）。
- 写请求到非 leader：返回 `ERR_LEADER_REDIRECT` + leader 地址（SDK 缓存并跟随）。
- **时间说明（v2 补充）**：状态机内不依赖墙钟做一致性判断；版本/审计时间戳取 leader 处理
  提案时的时钟（所有节点一致由日志序保证）；业务可配置 `--clock-drift-warn` 告警。

### 2.6 启动参数（极简配置 R12）

| 参数 | 说明 | 默认 |
|------|------|------|
| `--bootstrap` | 首节点自举 | 无（二选一必填） |
| `--join <endpoint>` | 加入集群（任一实例 HTTP 端点） | 无 |
| `--node-id` | 节点 ID（缺省自动生成并持久化） | 自动 |
| `--data-dir` | 数据目录 | `./data` |
| `--grpc-addr` / `--http-addr` / `--raft-addr` | 三端口 | 8383 / 8384 / 8385 |
| `--advertise-addr` | 对外公告地址 | 自动推断 |
| `--read-mode` | linear | stale | linear |
| `--publish-policy` | block | warn（发布校验） | block |
| `--shared-cascade` | auto | manual | auto |
| `--admin-password` | 初始管理员密码（缺省首启随机生成并打印一次） | — |
| `--master-key-file` / `DSH_MASTER_KEY` | 主密钥（§7） | 必配 |
| 其余 | 快照/日志/限额/保留策略等 | 见各节 |
| 配置优先级 | 环境变量（`DSH_*`）> 配置文件 > 命令行 | — |

## 3. 数据模型（详细）

### 3.1 逻辑模型（ER 文字版）

```
Project { id, name, created_at }                       // name 全局唯一
  ├─ Structure { version, groups[] }                   // 已发布结构（不可变，递增）
  │     groups[] = { name, items[] }
  │       items[] = { key, type, required, secret, validate }
  ├─ StructureDraft { base_version, groups[] }         // 未发布结构修改
  ├─ Branch { name, created_at }                        // 默认 dev/test/prod + 自定义
  │     └─ BranchState { active_version, last_request_id,
  │          value_draft: (group,key)→草稿值, versions: Version[] }
  │            Version { no, structure_version, created_at, operator, comment,
  │                      rollback_of?, kind: full|diff, snapshot_ref?, diff_ref? }
  └─ （引用已内嵌 ItemDef.shared_ref，无独立 RefBinding 实体）
SharedLibrary
  ├─ SharedDraft { group, key, type, secret, required, value }
  ├─ SharedItem { group, key, type, secret, required, value, version }
  └─ SharedVersion { no, created_at, operator, comment }   // 共享版本链（保留最近 N，默认 100）
AdminSession { token_hash, issued_at, expires_at, device_id }  // 全局唯一活动会话
AuditLog { seq, ts, operator, action, target, version?, request_id, detail }
```

### 3.2 存储模型（状态机 KV 前缀布局，RocksDB）

| Key 前缀 | 内容 |
|----------|------|
| `p/{pid}` | 项目元数据（name 唯一索引 `idx/pname/{name}`） |
| `p/{pid}/struct` / `p/{pid}/struct-draft` | 已发布结构 / 结构草稿 |
| `p/{pid}/b/{branch}/state` | 分支状态（active_version、value_draft、last_request_id） |
| `p/{pid}/b/{branch}/v/{no}` | 版本记录（full/diff，值见 §3.3） |
| `p/{pid}/refs/{g}/{k?}` | 共享库引用绑定（反查索引 `idx/ref/{shared_g}/{shared_k}`） |
| `sh/{g}/{k}` / `sh-draft/{g}/{k}` / `sh-vers/{g}/{k}/{no}` | 共享库 |
| `sess/admin` | 活动会话（单会话强制） |
| `audit/{seq}` | 审计日志（seq 递增） |
| `meta/…` | 集群元数据（openraft 管理） |

### 3.3 版本记录与 diff 存储格式
- 版本记录 JSON：
```json
{ "no": 12, "structure_version": 3, "created_at": "…", "operator": "admin",
  "comment": "fix redis host", "rollback_of": null,
  "kind": "full", "snapshot_ref": "p/order-service/b/prod/v/12/full" }
```
- diff（kind=diff）有序列表：`[{group, key, kind: upsert|delete, new_value}]`
  （new_value 为 proto `Value` 的 JSON 表示；secret 值为密文）。
- **checkpoint 规则**：每 100 版写一个 full；活动版本始终 full；历史版本 diff。
  读取历史：最近 checkpoint 起按序应用 diff。
- **保留策略**：默认全量保留；`--version-retention-count` / `--version-retention-days` 裁剪
  （只删历史，至少保留最近 1 个 checkpoint 之后）；裁剪由后台任务执行（§4.7），不阻塞写路径。

### 3.4 值类型、校验与限额（v2 新增限额）

| 类型 | 校验 | 存储 |
|------|------|------|
| string | 长度 ≤64KB、正则（validate 可配） | 明文 |
| int / float | 范围校验 | 明文 |
| bool | — | 明文 |
| json | 合法 JSON（规范化存储） | 明文 |
| array | 字符串数组，元素 ≤8KB | 明文 |
| secret | 强制加密（I8） | 密文（§7） |

| 限额（默认，可配） | 值 |
|------|------|
| 项目数 / 分支数(每项目) / 分组数(每项目) / item 数(每项目) | 10k / 100 / 500 / 10k |
| item 值大小 / 共享项值 / 版本 diff 大小 | 64KB / 64KB / 1MB |
| watch 订阅(每节点) / 事件缓冲(每订阅) | 10k / 1000 |
| 审计保留 | 100k 条或 30 天 |

### 3.5 结构强一致与结构草稿（v2 补充交互规则）
- 结构只存在项目级一份；结构修改 → 结构草稿；`PublishStructure` = 一次 Raft 写入：
  新结构版本 + 全部分支各推进一个版本（值不变）→ I3。
- **结构发布与值草稿的交互（D14）**：结构发布不丢弃值草稿；被删除 item 在值草稿中的值
  写入时清理（历史版本仍保留该值）；新增 item 的值草稿为空（required 发布时校验）。
- 新建分支：继承当前已发布结构 + 活动版本值（物化为初始值草稿）。

### 3.6 版本物化（materialization）
发布时把 ItemDef.shared_ref 解析后的共享值**写入版本快照**（版本自包含、不可变）；
共享库后续变更不影响历史版本；编辑/预览阶段才保持"活引用"。

## 4. 版本与发布引擎

### 4.1 分支值发布：事务伪代码（v2 新增）
```rust
// 在 Raft 提案 apply 内执行（I5：全部或全不）
fn apply_publish(p: PublishProposal) -> Result<Version, Error> {
    let draft = read_value_draft(p.project, p.branch);
    if draft.is_empty() { return Err(ERR_NO_DRAFT); }

    // 幂等：同 request_id 重复提案直接返回原结果（I10）
    let st = read_branch_state(p.project, p.branch);
    if st.last_request_id == p.request_id {
        return Ok(Version::at(st.active_version));
    }

    // 1) 校验：required / type / validate / 引用解析
    let errs = validate(draft, read_structure(p.project));
    if !errs.is_empty() && policy() == Block { return Err(ERR_PUBLISH_BLOCKED(errs)); }

    // 2) 物化：解析共享引用（失败 → ERR_VALIDATION）
    let resolved = materialize(draft, p.project);

    // 3) diff vs 活动版本；4) 写版本（full/diff 按 checkpoint 规则）
    let diff = compute_diff(read_active(p.project, p.branch), &resolved);
    let vno = st.active_version + 1;
    write_version(p.project, p.branch, vno, resolved, diff);

    // 5) 推进指针 6) 清空已发布草稿
    set_active(p.project, p.branch, vno);
    clear_consumed_draft(p.project, p.branch);

    // 7) 幂等记录 + 审计
    set_last_request_id(p.project, p.branch, p.request_id);
    audit(p.principal, "publish", /*…*/);

    // 事件扇出在提交后异步执行（§6），不阻塞 apply
    Ok(Version::at(vno))
}
```

### 4.2 幂等（D11，v2 新增）
- 所有管理写 API（publish/rollback/publish-structure/publish-shared/promote）必须携带
  `Idempotency-Key`（或 `client_request_id`）；缺失时服务端自动生成并随响应返回。
- 实现：状态机内 `last_request_id`（每 (项目,分支) 记录）用于发布类；CRUD 类用 leader
  内存窗口（10min）去重。重复请求返回首次结果（含同一版本号），不重复生效（I10）。
- 客户端重试策略：网络超时后携带同一 key 重试，安全。

### 4.3 结构发布
- `PublishStructure{comment, request_id}`：新结构版本 + 全部分支各推进版本（diff=结构变化），
  一次 Raft 写入；发布前 UI 展示影响预览（分支数、受影响 item）。

### 4.4 回滚
- `Rollback{project, branch, to_version, comment, request_id}`：读 to_version 快照（checkpoint
  重建）→ 创建新版本 no+1（rollback_of=to_version）→ 推进指针 → 事件（type=ROLLBACK）→ 审计。
- 历史不可变（I6/I9）；"再回滚"即再次 Rollback。

### 4.5 并发控制（v4.1 决策：单一管理员会话）
- 人工并发：I7 单会话（§9.3），第二个登录 `ERR_SESSION_IN_USE`。
- 程序化并发（CI 令牌）：发布为单 Raft 写入，天然串行；幂等键防重试重复。
- 企业版可加"发布锁"实现绝对串行（D4 旁注）。

### 4.6 发布前完整性校验
- 校验项：required 未填、类型/规则不符、引用解析失败、循环引用（共享库，DFS 判环）。
- 策略：`--publish-policy=block`（默认，`ERR_PUBLISH_BLOCKED` + 明细）|`warn`。

### 4.7 共享库发布与级联（v2 补全语义）
```rust
fn apply_shared_publish(p: SharedPublishProposal) -> Result<(), Error> {
    let draft = read_shared_draft();
    let errs = validate(draft);          // 含循环引用检测
    if !errs.is_empty() && policy() == Block { return Err(ERR_PUBLISH_BLOCKED(errs)); }

    let vno = shared_version + 1;
    write_shared_version(vno, /* 新值 */);

    if cascade_mode() == Auto {
        // 受影响 = 引用索引反查（idx/ref/{shared_g}/{shared_k}）
        for (pid, branch) in find_refs(draft.changed_keys()) {
            // 每个受影响分支：物化新共享值 → 新版本 → 推进 → 事件(SHARED_CASCADE)
            apply_cascade_to_branch(pid, branch, vno);
        }
        // 原子性：以上全部在同一提案内；任一步失败 → 整个提案失败（D15）
    }
    audit(p.principal, "shared_publish", /*…*/);
    Ok(())
}
```
- `--shared-cascade=auto`（默认）：共享发布自动级联，原子（D15）。
- `--shared-cascade=manual`：共享发布只更新共享库版本；引用项目在各自下次发布时物化新值
  （防风暴开关，D7）。

### 4.8 promotion 语义（v2 补全，D13）
- `POST /api/v1/projects/{p}/promote {from, to, items?}`：读 from 活动版本值 → 写入 to 的
  **值草稿**（发布后才生效）。
- 覆盖策略：目标草稿中已被本地修改的 item **默认跳过**（返回 skipped 列表）；`force=true` 覆盖。
- 返回 `{applied:[], skipped:[], missing_from:[]}`；操作审计；目标分支缺失项在发布时校验。

### 4.9 后台任务（v2 新增）
| 任务 | 周期 | 内容 |
|------|------|------|
| 版本裁剪 | 1h | 按保留策略删历史版本（保 checkpoint 保底） |
| DEK 重包 | 轮换后 | 用新 KEK 重加密所有 DEK（不重加密数据） |
| 审计清理 | 24h | 按 `--audit-retention` 裁剪 |
| 会话清扫 | 5min | 清除过期会话（单会话可用性兜底） |

## 5. API 与协议设计

### 5.1 传输与契约
- 数据面：gRPC；**正式契约：[proto/config.v1.proto](../proto/config.v1.proto)**（包 config.v1，
  含 GetConfig/GetItem/Watch/ListMembers、Value/Change/WatchEvent 等消息）。
- 管理面：REST `/api/v1`（JSON）；渲染与健康检查走 HTTP。
- Admin UI：同源 `/admin`（免 CORS）。
- 协议版本化：proto 包名带 major（config.v1）；REST 前缀 `/api/v1`；破坏性变更升 major。

### 5.2 数据面 API（见 proto 文件）
要点：GetConfig version=0 取活动版本；Watch after_version 断线续传；事件字段含
version/type/structure_version/comment/request_id/changes/snapshot_required。

### 5.3 管理面 API（REST，核心请求/响应 JSON）

| 方法 | 路径 | 请求体（节选） | 响应（节选） |
|------|------|---------------|-------------|
| POST | /api/v1/login | {password, device_id} | {token, expires_at, must_change_password} |
| POST | /api/v1/logout · /api/v1/heartbeat | — | {} |
| POST | /api/v1/projects | {name} | {id, name} |
| POST | /api/v1/projects/{p}/branches | {name} | {name, inherited_from_version} |
| PUT | /api/v1/projects/{p}/structure-draft | {groups:[…]} | {base_version, changed_items:[…]} |
| POST | /api/v1/projects/{p}/structure-draft/publish | {comment, request_id} | {structure_version, affected_branches:[…]} |
| PUT | /api/v1/projects/{p}/branches/{b}/draft | {updates:[{group,key,value}], deletes:[…]} | {draft_version} |
| POST | /api/v1/projects/{p}/branches/{b}/publish | {comment, request_id} | {version, structure_version, changes:[…]} |
| GET | /api/v1/projects/{p}/branches/{b}/versions | ?limit&before | {versions:[…]} |
| POST | /api/v1/projects/{p}/branches/{b}/rollback | {to_version, comment, request_id} | {new_version} |
| GET | /api/v1/projects/{p}/diff | ?branch_a&branch_b | {diffs:[…], missing:[…]} |
| POST | /api/v1/projects/{p}/promote | {from, to, items?, force?} | {applied, skipped, missing_from} |
| CRUD | /api/v1/shared, /shared-draft | {group, key, type, secret, value} | — |
| POST | /api/v1/shared/publish | {comment, request_id} | {version, affected:[…]} |
| GET | /api/v1/audit | ?action&since&limit | {entries:[…]} |
| POST | /api/v1/cluster/join · /promote · /remove | {node_id, …} | {members:[…]} |
| GET | /healthz · /readyz | — | {status} |
| GET | /v1/projects/{p}/branches/{b}/config | ?format=yaml|toml|json&version=&reveal= | 渲染文档 |

- 所有写操作响应头携带 `Idempotency-Key`（服务端生成或回显客户端值，§4.2）。

### 5.4 错误码（与 proto 注释一致）

| 错误码 | 含义 | 客户端动作 |
|--------|------|-----------|
| ERR_LEADER_REDIRECT | 非 leader（携带 leader_hint） | SDK 跟随并缓存 |
| ERR_NOT_FOUND | 不存在 | — |
| ERR_VALIDATION | 校验失败（带明细） | 展示 |
| ERR_PUBLISH_BLOCKED | 完整性校验阻断 | 展示明细 |
| ERR_VERSION_PRUNED | 续传起点已裁剪 | 重拉全量 |
| ERR_SESSION_IN_USE | 已有管理员在线 | 等待/强制下线 |
| ERR_SESSION_EXPIRED | 会话过期 | 重新登录 |
| ERR_FORBIDDEN | 无权限 | — |
| ERR_CYCLE_REF | 共享引用成环 | 提示 |
| ERR_CONFLICT | 乐观锁冲突 | 刷新重试 |
| ERR_NO_DRAFT | 无待发布草稿 | 提示 |
| ERR_LIMIT_EXCEEDED | 超限额（§3.4） | 提示 |

## 6. Watch 与推送

### 6.1 事件模型（proto WatchEvent）
- 事件只由发布产生（I4）；字段：version、type（VALUE_PUBLISH/STRUCTURE_PUBLISH/SHARED_CASCADE/
  ROLLBACK）、structure_version、comment、request_id、changes[]、snapshot_required。

### 6.2 订阅生命周期（v2 补充 keepalive）
1. `Watch{project, branch, after_version=0}`：先返回当前版本号（SDK 用 GetConfig 拉全量）。
2. 持续推送后续发布事件（有序，版本号严格递增）。
3. **keepalive**：无事件 30s 服务端发 keepalive（gRPC ping/空事件）；客户端 60s 无数据
   判定断线 → 重连 `after_version=last_version`。
4. 重放：服务端按版本链重放 last_version 之后的事件；起点被裁剪 → `ERR_VERSION_PRUNED`
   （或发 snapshot_required 事件）→ SDK 重拉全量。

### 6.3 服务端实现
- 订阅表：每节点本地 `HashMap<(pid,branch), Vec<WatchStream>>`；事件在 leader 提交后广播
  到所有节点，各节点扇出给本地订阅者（容错 follower 上的订阅）。
- 事件日志保留：最近 10k 事件（`--watch-event-retain`）供重放；重放优先用版本链。
- **慢消费者（v2 新增）**：每订阅缓冲 1000 事件；溢出 → 发 `snapshot_required=true` 并关闭
  流，SDK 重拉全量（防内存爆炸）。

### 6.4 推送通道
- gRPC 服务端流（首选）；长轮询（代理穿透）；SSE（TS 补充）。三通道同事件模型。

## 7. 加密设计

### 7.1 威胁模型
- 覆盖：磁盘/快照/备份被读 → 密文；传输由 TLS 保护。
- 不覆盖：内存侧信道、恶意管理员、KMS（企业版）。明文仅存在于解密瞬间（I8）。

### 7.2 算法与密钥层次（信封加密）
```
主密钥 KEK（32B，env/文件/KMS，仅内存）
  └─ 每 item 数据密钥 DEK（随机 32B，随密文存储）
       └─ 密文 = AEAD_enc(DEK, nonce, 明文)
  附：encrypted_DEK = AEAD_enc(KEK, nonce2, DEK)
```
- 算法：AES-256-GCM（首选，AES-NI）或 ChaCha20-Poly1305（软件场景）。

### 7.3 密文 wire 格式（v2 明确）
item 存储 JSON：
```json
{ "enc": "aes-256-gcm", "v": 1, "dek_v": 3,
  "nonce": "b64", "ct": "b64",
  "edek": "b64", "edek_nonce": "b64" }
```
- dek_v：DEK 版本（轮换用）；edek = KEK 加密的 DEK。版本快照/diff 中同格式。

### 7.4 主密钥来源
- `DSH_MASTER_KEY`（base64 32B）或 `--master-key-file`（raw 32B/PEM，权限 0400）。
- 口令 → Argon2id 派生（企业版 `--master-key-passphrase`）。
- `dsh admin gen-master-key` 生成并打印指引；主密钥**不明文落盘**。
- **启动检查（v2 新增）**：无主密钥拒绝启动（含 secret 的场景必配；无 secret 项目可
  `--allow-no-master-key` 开发模式）。

### 7.5 轮换
- `dsh admin rotate-master-key`：换 KEK → 后台任务用新 KEK 重包全部 DEK（edek 更新，数据不重加密）。
- 轮换期间：新写用新 KEK；旧 KEK 保留在内存列表直到全部重包完成（可解旧数据）。

### 7.6 脱敏与审计
- 管理面/导出默认掩码（`***`）；`reveal=true` 需权限 + 审计。
- 解密/导出/含 secret 版本的回滚 → 审计条目（操作者/时间/目标/版本）。

## 8. 多格式渲染

### 8.1 IR 与渲染约束
- IR = 版本物化后的 `map<group, map<key, Value>>`；以 TOML 为表达力下限：
  键限标识符/引号串；顶层表=分组；json 类型输出为内联表/数组或字符串（规则文档化）；
  无 null（secret 未填 → 注释占位）。
- 渲染：serde_json / serde_yaml / toml。

### 8.2 输出路径的 secret 语义（v2 明确）
- **SDK GetConfig / 渲染给应用**：解密后的真实值（经 TLS）。
- **管理面导出/预览**：默认掩码；`reveal=true` 需权限 + 审计。
- 文件名：`{project}-{branch}-v{version}.{yaml|toml|json}`；打包 `{…}-all.zip`。

### 8.3 引用解析与等价性
- 渲染/发布时解析 ItemDef.shared_ref；悬空引用阻断（结构保存/发布已前置校验）。
- 等价性测试：随机 IR → 三格式 → 解析 → 规范化比较（§14）。

## 9. Admin UI

### 9.1 技术选型
- React + TS + Vite（产物 ≤5MB 基准）；rust-embed 编译进二进制；axum `/admin` 静态 +
  SPA fallback；CSP、无外链。

### 9.2 页面结构
登录 → 项目列表 → 项目详情（分支 Tab）→ 分支编辑（树形值编辑器 + **待发布变更视图** +
**发布确认**（校验结果 + 影响预览））→ 版本历史（列表 + diff + **回滚确认**）→ 分支对比 +
promote → 共享库（CRUD + 引用 + 级联预览）→ 审计 → 设置（主密钥/保留/成员/强制下线）。

### 9.3 单一管理员会话（I7，v2 补充初始凭证引导）
- 登录成功 → 会话（token 内存 + 状态机 `sess/admin`：token_hash/expires_at/device_id）。
- 第二个登录 → `ERR_SESSION_IN_USE`：UI 提示"已有管理员在线（设备/时间）" + "强制下线"按钮
  （二次确认，替换会话）。
- 心跳每 5min 续期；TTL 24h（`--session-ttl`）；空闲 30min 可配；后台任务清扫过期会话。
- CLI 兜底：`dsh admin force-logout`。
- **初始凭证引导（v2 新增）**：`--admin-password` 显式设置；否则首启生成随机密码打印一次
  + `must_change_password=true`（首次登录强制修改，改后旧会话失效）。

### 9.4 权限分离
- 数据面：API 令牌（只读/watch）；管理面：会话（读写）。MVP 无 RBAC；企业版分支级权限/SSO/MFA。

## 10. SDK 契约（TS / Go / Python）

### 10.1 共同行为
- `ConfigClient(endpoints, {tls, token})`；端点池 + failover（失败切换 + 指数退避
  500ms→30s + 抖动）；`ERR_LEADER_REDIRECT` 跟随 leader_hint 并缓存。
- `get(project, branch) / getItem(project, branch, group, key) / watch(project, branch, listener) / close()`。
- watch：首连拉全量 → 事件增量更新本地缓存 → 回调（版本号严格递增，保证顺序）；
  断线 `after_version` 续传；`snapshot_required`/VERSION_PRUNED → 重拉全量。
- 重试矩阵（v2 新增）：`NOT_FOUND`不重试；`VERSION_PRUNED`重拉；`SESSION_IN_USE/EXPIRED`
  仅管理面；网络错误重试（幂等键配合）。

### 10.2 语言差异
- TS：浏览器（SSE/WebSocket）+ Node（gRPC）；类型化事件。
- Go：gRPC + goroutine 扇出；context 贯穿。
- Python：async（grpc.aio）；listener 为 async 回调；线程安全缓存。

### 10.3 契约测试
三语言 SDK 对同一 Rust mock 服务（golden 协议）跑同一套用例：failover、leader 重定向、
watch 顺序、断线重放、VERSION_PRUNED、慢消费者恢复、secret 脱敏、幂等重试、多格式获取。

## 11. 安全设计

| 项 | 设计 |
|----|------|
| 传输 | gRPC/HTTP 支持 TLS；生产建议强制 |
| 认证 | 管理面会话（单会话）；数据面 API 令牌（只读/watch） |
| 鉴权 | MVP 全局管理员；企业版分支级 RBAC |
| 会话 | TTL + 心跳 + 强制下线 + 登录失败限次（5 次/10min 锁定）+ 设备绑定 |
| 幂等与重放 | Idempotency-Key（I10）防重复提交 |
| 密钥 | §7 信封加密；主密钥不明文落盘；无主密钥拒绝启动 |
| Web | CSP、XSS/CSRF 防护、初始凭证强制修改、无外链 |
| 审计 | 登录/登出、发布/回滚/结构发布/共享发布、解密/导出、promote、强制下线、成员变更 |
| 供应链 | cargo deny + RustSec；前端 lockfile + SBOM；产物哈希固定 |

## 12. 可观测性

- `/healthz`（存活）、`/readyz`（已加入集群且可服务）。
- Prometheus 指标：`raft_role/leader/term/committed_index/snapshot_size`、
  `api_qps/api_latency{grpc,http}`、`watch_conns/watch_events_total/watch_dropped`、
  `publish_total/rollback_total/versions_total/drafts_pending/shared_refs_total`、
  `storage_bytes/session_active/master_key_ok`；延迟分桶（`0.5/1/5/10/50/100/500/1000ms`）。
- 结构化日志：JSON 可选；字段：`ts, level, request_id, operator, action, project, branch, version`。
- 审计条目（v2 明确 schema）：
```json
{ "seq": 1024, "ts": "…", "operator": "admin", "action": "publish",
  "project": "p", "branch": "prod", "version": 12, "request_id": "r-9",
  "detail": { "policy": "block", "changes": 3 } }
```

## 13. 配置与部署

### 13.1 配置示例（dsh.yaml）
```yaml
bootstrap: true
data_dir: ./data
grpc_addr: ":8383"
http_addr: ":8384"
raft_addr: ":8385"
read_mode: linear
publish_policy: block
shared_cascade: auto
session_ttl: 24h
master_key_file: /run/secrets/dsh-master-key
version_retention_count: 0        # 0=全量保留
watch_event_retain: 10000
```

### 13.2 部署与运维
- Docker：静态编译镜像；`docker-compose.yml` 一键三节点（bootstrap + 2×join + 3 数据卷 + secret）。
- 备份：快照 + 日志归档（`dsh admin snapshot`）；恢复：`--data-dir` 指向备份 + rejoin。
- **CLI 命令清单（v2 新增）**：`dsh admin` 子命令：`gen-master-key / rotate-master-key /
  force-logout / set-password / remove-node / promote / snapshot / version-retention-status`。
- 升级：滚动升级（成员逐个替换）；proto major 变更需停服迁移（文档化）。

## 14. 测试与质量（v2 扩充）

| 层 | 内容 |
|----|------|
| 单元 | 数据模型、校验器、diff、加密 KAT 向量、渲染器 |
| 集成 | API 全链路、发布事务、级联、回滚、幂等 |
| Raft 故障注入 | 分区/丢包/乱序/kill/重启/快照追赶/成员变更 |
| watch | 断线重放、VERSION_PRUNED、**慢消费者恢复**、keepalive、顺序 |
| 幂等 | 同 request_id 重复发布/回滚只生效一次（I10） |
| 级联 | auto/manual 两种模式、**原子性**（部分失败整批回滚）、引用环 |
| 加密 | 轮换、DEK 版本化、旧 KEK 兼容、**磁盘无明文扫描** |
| 等价性 | 随机 IR → 三格式语义等价（属性测试） |
| 限额 | 超限边界（值大小、订阅数、事件缓冲） |
| 契约 | 三 SDK vs mock 同一套用例 |
| 混沌/基准 | 写 QPS ≥10k、watch ≥10k、发布→SDK ≤1s、内存 ≤128MB、二进制 ≤50MB |

## 15. 里程碑与 M0 检查清单（v2 新增）

| 阶段 | 交付物 |
|------|--------|
| M0（2 周） | 契约文件（proto/config.v1.proto + REST 文档）、数据模型定稿、决策记录闭合（§16）、CI 脚手架 |
| M1（4~6 周） | raft-node（bootstrap/join/promote/remove/快照）、core（CRUD+草稿）、storage、配置 |
| M2（4~6 周） | publish 引擎（含幂等）、结构发布、共享库+级联、diff/promote、crypto、render、jobs |
| M2.5（3~4 周） | admin-ui（单会话/草稿/发布/历史/回滚）、watch 服务端、可观测性 |
| M3（4~6 周） | 三 SDK + 契约测试 |
| M4（持续） | 混沌、加固、文档、发布、compose 示例 |

**M0 检查清单**：□ §16 决策记录逐项确认（默认值即基线） □ proto 文件评审通过
□ 错误码表与各模块引用一致 □ 限额表定稿 □ 不变量→测试映射（§18.2）评审 □ 数据模型
（ER + KV 布局）评审 □ CI 脚手架（构建 + 契约 golden 测试骨架）。

## 16. 决策记录（v1 §16 全部闭合，v2 新增 D11–D16）

| # | 决策 | 结论（默认即基线） | 理由 |
|---|------|-------------------|------|
| D1 | 回滚粒度 | **整版本回滚** | 简单可审计；单 item 回滚列后续 |
| D2 | 发布校验默认 | **block**（可配 warn） | 防止"漏配上线"（P11） |
| D3 | 版本存储 | **快照 + diff（每 100 版 checkpoint）** | 读快/省存储/历史可重建 |
| D4 | 灰度发布 | **企业版第一期** | 不进开源 MVP |
| D5 | 会话 | **TTL 24h / 心跳 5min / 空闲 30min 可配** | 单会话可用性兜底 |
| D6 | 单会话语义 | **拒绝第二个登录 + 强制下线按钮/CLI** | v4.1 用户决策 |
| D7 | 共享级联 | **auto（原子）＋ manual 开关** | 默认自动化，风暴可关 |
| D8 | 存储引擎 | **RocksDB（rust-rocksdb）** | sled 停维护 |
| D9 | 共识库 | **openraft**（备用 raft-rs） | 现代 API、learner/成员变更内建 |
| D10 | 分组嵌套 | **平铺一层** | MVP 简化；嵌套列后续 |
| D11 | 幂等 | **所有管理写操作支持 Idempotency-Key**（I10） | 网络重试安全 |
| D12 | 限额 | **§3.4 限额表默认值** | 防滥用/防内存爆炸 |
| D13 | promote 覆盖 | **默认不覆盖目标草稿已改项，force 可覆盖** | 防误覆盖 |
| D14 | 结构发布 vs 值草稿 | **结构发布不丢值草稿；删 item 清其草稿值** | 语义明确 |
| D15 | 级联失败 | **原子：整批生效或整批失败** | 可解释、无半级联 |
| D16 | 时间 | **状态机不依赖墙钟做一致性判断** | 防时钟漂移影响共识 |

## 17. 审核与修订记录（v1 → v2）

| 发现类别 | 问题 | v2 处理 |
|----------|------|---------|
| 决策未闭合 | §16 开放点 D1–D10 无结论 | §16 全部闭合为默认 + 理由 |
| 契约不完整 | §5 仅有伪代码/表格 | 正式 [proto/config.v1.proto](../proto/config.v1.proto) + REST JSON schema（§5.3） |
| 一致性 | revision 术语仅在术语表出现 | 明确 revision 内部化，对外统一版本号 |
| 缺口 | 无幂等设计 | D11 + §4.2 实现与重试矩阵（§10.1） |
| 缺口 | 无限额，watch 慢消费者未定义 | §3.4 限额表 + §6.3 慢消费者策略 |
| 缺口 | 无成员移除/节点重启身份 | §2.1/§2.2 remove、identity 持久化、rejoin |
| 缺口 | 级联语义不完整（失败/原子性/手动模式） | D7/D15 + §4.7 伪代码 |
| 缺口 | 无初始凭证引导、无 CLI 清单 | §9.3、§13.2 |
| 缺口 | 无备份/恢复、无配置示例 | §13.1/§13.2 |
| 缺口 | 输出路径 secret 语义未区分 | §8.2（SDK 解密 vs 管理面掩码） |
| 缺口 | 无后台任务（裁剪/重包/清理） | §4.9 jobs 模块 |
| 缺口 | 密文 wire 格式未定义 | §7.3 |
| 缺口 | 审计 schema/指标分桶未定义 | §12 |
| 缺口 | M0 无检查清单 | §15 M0 检查清单 |
| 不变量 | 无幂等不变量 | I10 + §18.2 测试映射 |

## 18. 附录

### 18.1 仓库结构（v1 沿用，新增 proto/）
```
repo/
  server/        # Rust 工作区（crates：core/raft-node/storage/publish/api/watch/crypto/render/admin-ui/observability/jobs）
  proto/         # config.v1.proto（正式契约）
  sdk/ts/ sdk/go/ sdk/python/
  deploy/        # Dockerfile + docker-compose.yml
  docs/          # 需求/可行性/设计文档
```

### 18.2 不变量 → 测试映射（v2 扩充）
- I1/I2 → Raft 故障注入套件 ｜ I3 → 任意操作后断言全分支结构恒等
- I4 → 草稿编辑期间 GetConfig 不变 ｜ I5 → 发布断点注入无半发布状态
- I6 → 发布后修改旧版本被拒 ｜ I7 → 并发登录第二个收到 ERR_SESSION_IN_USE
- I8 → 磁盘/快照/备份无明文扫描 ｜ I9 → 回滚审计完整
- I10 → 同 request_id 重复发布只生效一次

### 18.3 参考
- [docs/proposal-v4.md](./proposal-v4.md)、[docs/feasibility-report.md](./feasibility-report.md)、[docs/design-v1.md](./design-v1.md)
- [proto/config.v1.proto](../proto/config.v1.proto)
- [openraft](https://github.com/databendlabs/openraft)、[raft-rs](https://github.com/tikv/raft-rs)、[axum](https://github.com/tokio-rs/axum)、[tonic](https://github.com/hyperium/tonic)
- [rust-embed](https://docs.rs/rust-embed)、[aes-gcm](https://docs.rs/aes-gcm)、[etcd API guarantees](https://etcd.io/docs/v3.8/learning/api_guarantees/)
