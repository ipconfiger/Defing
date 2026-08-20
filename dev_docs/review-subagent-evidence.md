# 子代理深读报告 · 附录（dsh-api/dsh-raft × SDK/UI/契约测试）

> 归档日期：2025-08-16 ｜ 来源：深度分析（dev_docs/deep-analysis-2025.md）期间的
> 两个并行只读子代理，行级证据原文存档。主报告的 F1–F20 编号与本节对应。

---

## 附录 A：dsh-api / dsh-raft 深读（分析范围：lib.rs 3207 行、grpc.rs、raft/{store,raft,http_network,raft_http_server,network,types} + 支撑文件）

### A. 架构与代码质量

**模块边界（清晰）**：dsh-api=HTTP/gRPC 面、dsh-raft=openraft 集成、dsh-core=确定性状态机、
dsh-publish=发布编排、dsh-watch=扇出、dsh-crypto/dsh-storage/dsh-observability/dsh-jobs/dsh-cli。
API 层不触碰状态机业务，写路径统一收敛到 `dsh_raft::write_command`（raft.rs:161-215）+
`ApiState::write`（lib.rs:129-140）。PublishService 只做"提交前加密 + 组命令"，确定性 apply 留在 core，边界正确。

**错误处理**：统一 `ApiError(dsh_core::Error)` 包装 + 状态码映射表（lib.rs:159-187），映射基本合理；
gRPC 侧独立 map_err（grpc.rs:305-314）。缺陷：① 读路径约 20 处 `expect("sm lock")`
（lib.rs:329,509,527,583,785,921,991,1227,1356,1401,1538,1639,1665,1774,1876,2040,2113,2331,2757,2790…），
Mutex 中毒即请求级 panic；② `LeaderRedirect` 映射为 409（lib.rs:172），与真实 409 语义混淆；
③ login/rotate 的 leader 转发错误体用 `resp.json().await.unwrap_or(json!({}))` 兜底
（lib.rs:1967,2164,2563），非 JSON 响应时错误码丢失。

**并发**：全局单把 `std::sync::Mutex<StateMachine>` 串行化读写（store.rs:399-400）——简单但吞吐受限，
读多写少无并发读。锁作用域基本为语句级 `{}` 块，未见跨 await 持锁；
`StateMachineStore::apply` 持锁期间做广播 send（非阻塞）与 rotation hook 文件 I/O（store.rs:543-571），
锁内 IO 为性能/阻塞隐患，非死锁。watch 用 1024 容量 broadcast（store.rs:422）。

**重复代码（热点）**：login（lib.rs:1943-2013）、pa_login（2142-2204）、rotate_master_key（2543-2590）
三份几乎逐字相同的"LeaderRedirect→补 http://→reqwest 转发→透传错误体"样板；
`plain_value`/`plain_groups`/`apply_secret_policy`/`masked_shared_value` 四个掩码实现并存
（lib.rs:1077-1083,1365-1384,1675-1708）——正是 F1/F2 漏掩码的温床。

**复杂度热点**：lib.rs 单文件 3207 行过重；`rotate_master_key`（2509-2667）与 `login`（1839-2028）
函数过长。store.rs 的 redb 表句柄 drop 顺序依赖人工维护（store.rs:189-197,200-231，注释细致，正确）。

**可读性/注释**：注释质量高——中文设计引用、N/S/C 修复编号全程可追溯、redb 事务语义注释到位。
**unsafe**：业务代码零 `unsafe`。

### B. API 面清单

**HTTP 路由（build_router，lib.rs:2959-3047）** 与 openapi.v1.yaml（36 条）对照：
- **未在 openapi 声明**：`/api/v1/projects/{p}/admins`（GET/POST，lib.rs:2980-2982）、
  `/api/v1/projects/{p}/admins/{u}`（DELETE/PUT，2983-2986）、`/admin/{*path}`（2965）。
  PA 账号管理端点属安全敏感面，文档缺失属中等文档漂移。
- 其余 33 条均与 openapi 对齐。

**鉴权豁免点（auth_middleware，lib.rs:423-472）**：
1. `/healthz`、`/readyz`、`/api/v1/login`、`/api/v1/cluster/join` 显式豁免（432-437）；
2. **所有非 `/api/v1/` 前缀路径全部无鉴权**：`/metrics`、`/admin*`、数据面
   `/v1/.../{snapshot,config,watch}`（439 行只检查 `/api/v1/`）。`render_config` 在 reveal=true 时
   手动补鉴权（1739-1771），`snapshot`/`watch` 完全开放（design 已知偏差 D2）。

**gRPC（grpc.rs）**：4 RPC 质量良好——锁粒度正确、错误映射合理、**gRPC watch Lagged 处理正确**
（snapshot_required+关流，grpc.rs:255-267）。问题：`get_item` 每次拉全量快照再取单键
（160-171，O(项目全量)）；watch 重放对每条版本做全量快照 diff（200-226，O(n·库大小)）；
`list_members` 的 `grpc_addr` 在 join 时被置空（lib.rs:2390）。数据面鉴权 `data_plane_interceptor`
默认放行（grpc.rs:28-40；main.rs:109-110）。

### C. 鉴权与授权

**会话校验（resolve_principal，lib.rs:324-357）**：token 前缀路由 `pa.{username}.{secret}`→PA、
其余→admin；PA 的 project 归属以状态机存储的 principal 为准（352-353，防伪造）✓；
过期校验 `now_ms()<expires_at`（348）✓；token 哈希存状态机（state.rs:1731-1739）✓。
PA 用户名字符集 `[A-Za-z0-9_-]` 无点（state.rs:1554-1562），前缀分割无歧义 ✓。

**pa_allowed 矩阵（lib.rs:377-420）**：默认拒绝、显式放行；显式拒绝=跨项目、`/admins*`（405-410）、
DELETE 自身项目（412-414）、非项目路径（418-419）。`project_segment`（361-373）对未解码原始路径做
严格 `[a-z0-9-]` 校验，`%2F`/`%2e` 编码绕过被字符集封死 ✓。PA 对共享写/集群/全局 admin 端点 403 ✓。

**join 端点（lib.rs:2357-2408）**：`join_token_ok` 在 `join_token=None` 时恒 true（2359-2360）；
main.rs:111-113 默认 None → **默认部署 join 完全开放**（F3）。无 node_id 唯一性/地址可达性校验（2394）。

**数据面 token**：gRPC 可配 `data_plane_token`，HTTP 数据面 `/v1/*` 无任何 token 机制——
同一数据面两套鉴权策略，HTTP 侧不可配（D2 已知）。

**发现**：HTTP watch 密文泄漏（F1）；branch_diff 密文泄漏（F2）；登录节流键 XFF 可伪造（F4）；
集群 login 转发不回传 XFF → leader 侧失败全记 "direct" 键。

### D. 正确性隐患

**apply 确定性（良好）**：时间戳命令载荷携带（C2 修复，state.rs:423-429）；SessionInUse 只判 is_some
（state.rs:1522,1636-1637）；`apply_rotate_master_key` 不落数据（state.rs:1725-1727）；加密在 apply 外
（publish.rs:70-105）✓。apply 错误经 R=Result<u64,Error> 返回（types.rs:30-37，不再吞错）✓。
残余：`encrypt_secret_updates` 按**接收节点** structure 判定 secret 性（publish.rs:78-104），
节点结构滞后时 secret 可能明文进 Raft 日志（低，竞态）。

**快照**：persist meta+data 同事务原子（store.rs:44-67）；内存→盘回退（627-644）；
install 落盘+last_applied/membership（602-625）。边角：last_applied 仅 `if let Some` 写（617-619）；
snapshot_id=index-len 在 index 相同且字节数相同时碰撞（内容必然相同，实际无害）。

**watch 扇出**：`spawn_raft_forward` 集群模式正确接线（main.rs:513）；`sm_store.events` send 失败被
`let _` 忽略（store.rs:563）——1024 容量溢出时集群内转发静默丢事件、无 snapshot_required（低）。

**leader 转发**：仅 login（1907-2027）与 rotate（2535-2638）自动跟随；**其余全部写 handler** 在
非 leader 节点直接 409+leader_hint，由客户端自行跟进。转发体 `reqwest::Client::new()` **无超时**（F8）。

**panic 风险清单**：expect("sm lock")×20+；`serde_json::to_value(...).expect`（588,739,1669,2345——
Float NaN 理论可触发，实际 serde_json 输入不可产生，低）；`key[..8]` 切片（store.rs:94-98，定宽安全）；
`Bound::Excluded(0)` → `*i-1` 下溢 u64::MAX（store.rs:267-270，性能边角）；admin_static 的 expect（低）。

**静默吞错**：HTTP watch 重放 `if let Ok`（lib.rs:2841-2866，C5 已承认）；watch_sse Lagged 丢弃（F5）；
`let _ = self.events.send`（store.rs:563）；AuditLog 尽力而为（design 承认）；decrypt 失败→"***"
（1374-1377，安全但静默）；update_draft deletes 无 `/` 条目静默丢弃（lib.rs:670-677）。

### E. 与 code-review.md 声明修复项核对

| 项 | 结论 | 证据 |
|----|------|------|
| S2 join 鉴权 | **机制在、默认关（F3）** | join_token=None→恒放行（2359-2360）+ CLI 默认 None（main.rs:113） |
| S3 RotateMasterKey 经 Raft | ✅ 已实现 | command.rs:189-192；store.rs:567-571 幂等钩子；main.rs:332-358 先持久化后切换。副作用：KEK 明文进 Raft 日志（F7） |
| S5 raft RPC token | **机制在、默认关** | raft_http_server.rs:28-51；token=None 保持无鉴权（19-20）；CLI 默认 None（main.rs:114-116） |
| R1 超时 | ⚠️ raft 侧已修、API 转发漏修（F8） | http_network.rs:113-116 connect 3s+total 60s；RPCOption 被忽略（62）；login/rotate 转发无超时 |
| C1 operator 透传 | ✅ 已修复 | publish.rs:124,150,192,223；残余硬编码 lib.rs:483/823/1129（admin-only，语义正确） |
| C2 时间戳注入 | ✅ 已修复 | command.rs 8 变体 ts；state.rs:423-429；apply fallback=log_id.index（store.rs:557）仅作用旧日志 |
| S6 节流+argon2 | ✅ 已实现 | lib.rs:2877-2932；1452-1475；缺陷：节流键可伪造（F4） |

### F. 新发现缺陷（编号与主报告一致）

- **F1（高）HTTP watch SSE 对未鉴权客户端泄漏 secret 密文**：`/v1/.../watch` 无鉴权
  （lib.rs:439 只覆盖 /api/v1/；路由 3033）。`watch_branch`（2829-2871）+ `watch_sse`
  （dsh-watch/src/lib.rs:57-87）直接序列化 `PublishEvent`，changes 里 `Value::Secret` 输出
  `{"type":"secret","ciphertext":...}`（model.rs:137-140）。重放（2847-2863）与实时（store.rs:561-564）
  均裸透。对照 gRPC（grpc.rs:59-62,113）与 HTTP snapshot（1651）均掩码——唯独 HTTP watch 漏，
  违反 design §7.6。密文还可作相等性 oracle。
- **F2（高）`branch_diff` 泄漏 secret 密文**：lib.rs:946-950 将 `Value::Secret(ct)` 原样放入
  branch_a/branch_b。PA 可对自己项目调用（pa_allowed 放行 400-417），绕开掩码策略；
  可自写 secret 后跨分支比对探测相等性。
- **F3（中）S2 修复默认不生效**：join_token 默认 None → 任意网络可达者仍可注册 learner
  拉走全量 Raft 日志（密码哈希/会话哈希/密文）。
- **F4（中）登录节流键可伪造（XFF）**：lib.rs:1846-1853 取 X-Forwarded-For 首值无可信代理配置：
  ① 伪造 IP 绕过节流；② 受害 IP 连错 5 次被锁 600s（DoS）；③ 集群 login 转发不回传 XFF（1958-1962），
  leader 把全部失败记 "direct" → 5 次失败集群级锁死直连登录。
- **F5（中）HTTP watch 慢消费者静默丢事件、流不结束**：`watch_sse` 的 `item.ok()?` 把 Lagged 丢弃
  （dsh-watch/src/lib.rs:71-84），与注释"流结束"（55-56）及 gRPC 正确行为（grpc.rs:255-267）矛盾。
- **F6（中）集群模式写响应 events 恒空 → changes/affected 缺失**：`write_command` 集群分支
  `events: vec![]`（raft.rs:189-192）→ publish `changes:[]`（739）、publish_structure
  `affected_branches:[]`（643-647）、publish_shared `affected:[]`（1211-1225）——dev-single 与集群不一致。
- **F7（中）KEK 明文入 Raft 日志且 redb 落盘权限未收紧**：`RotateMasterKey { kek }`
  （command.rs:189-192）明文写 raft-log（store.rs:347-351）；redb 数据文件创建未设 0600
  （dsh-storage/src/lib.rs:62-70，通常 0644）。S4 只修了 ring 文件，主密钥 at-rest 暴露面扩大。
- **F8（中）login/rotate 转发客户端无超时**：lib.rs:1952,2151,2552 `reqwest::Client::new()`；
  10s deadline（2016,2207,2594）只在 await 返回后检查——R1 漏修 API 转发路径。
- **F9（中）secret 共享项非 String 值不加密、明文级联**：`write_shared_draft` 仅对 Value::String 加密
  （lib.rs:1103-1115）；secret:true + int/json 明文落共享草稿 → SharedPublish 级联进项目分支
  （state.rs:1203+）→ 数据面不掩码（1372 只处理 Secret 变体）→ 明文 secret 暴露。
- **F10–F20（低）**：openapi 缺 PA 账号路由与 /admin/{*path}；LeaderRedirect→409 语义混淆；
  CSP 仍含 'unsafe-inline'；expect("sm lock")×20；join 无 node_id 唯一性/地址可达校验；
  deletes 无 `/` 静默丢弃；HTTP watch 重放吞错（C5 残留）；gRPC watch 重放 O(n) 全量 diff；
  install_snapshot last_applied 条件写；sm_store.events 溢出无感知；硬编码 operator "admin"（admin-only）。

---

## 附录 B：SDK × Web 控制台 × 契约测试深读

### A. 三语言 SDK API 一致性

**一致**：方法面、Snapshot/WatchEvent 形状、ty 映射（与 model.rs:298-306 snake_case 及 proto 枚举对齐）、
secret 脱敏 "***"、after_version 断线续传——三语言均实现。

**不一致 / 契约未对齐**：
1. **GetItem RPC 是死代码**：三 SDK 的 getItem 全部用「GetConfig 全量快照 + 本地查找」
   （grpc.ts:114-117、grpc_client.go:105-116、config_client.py:115-119），服务端 GetItem RPC
   （grpc.rs:153-177）零调用、not_found 语义零覆盖。
2. **version 参数在 HTTP 通道被静默丢弃**：TS `get(p,b,version)` HTTP 路径不拼 version（index.ts:131-133）；
   Python 同（config_client.py:113）；Go HTTP Get 无 version 参数。只有 gRPC 支持版本读取。
3. **错误类型三语言不统一**：TS 包装 ConfigError；Go 裸 error（HTTP `fmt.Errorf("GET %s -> %d")`、
   gRPC 裸 RpcError）；Python 仅 NO_ENDPOINT/NO_GRPC 包装。跨语言错误码对齐不存在。
4. **重试/退避参数不一致**：普通请求退避 TS 200ms/Go 100ms/Python 200ms 基线；watch HTTP 退避
   TS/Go `min(1000*2^n,15s)`、Python `min(200*2^n,15s)` 差 5 倍；**Python gRPC watch 退避恒 400ms 常数**
   （config_client.py:184），TS/Go gRPC 重连固定 1s（grpc.ts:149、grpc_client.go:149,161），均非指数。
5. **HTTP 错误不触发端点 failover**：4xx/5xx 直接 throw（index.ts:112-115、client.go:76-77）；
   watch 三语言只用 endpoints[0]（index.ts:204、client.go:127、config_client.py:196）——「端点池 failover」声明过强。
6. **secret 解码分歧（潜伏）**：Go 无条件 "***"（grpc_client.go:70-72）；TS 看 masked 标志（grpc.ts:48-50）；
   Python 不看 masked 只按 oneof 返回（config_client.py:26-40）。当前服务器恒脱敏行为恰好一致；
   一旦实现 openapi 描述的"解密为真实值"，Go 仍脱敏、Python 直出真值。
7. **gRPC 事件缺 project/branch**：TS gRPC 通道事件 project:''/branch:''（grpc.ts:82-83）；
   proto WatchEvent 本就无此字段，消费方依赖 e.project 会踩坑（Go/Python 同）。
8. **listMembers 返回形状不一致**：TS 裸 proto message（snake_case）；Go PascalCase struct；Python snake_case dict。
9. **空端点列表**：Python 构造时 endpoints[0] 直接 IndexError（config_client.py:62），TS/Go 请求时才报。
10. **契约文档三处自相矛盾**（服务端侧）：
    - openapi `/snapshot` 描述「secret 解密为真实值」（yaml:607）vs 实现恒掩码（lib.rs:1651,1690）；
    - openapi ConfigSnapshot schema 嵌套 Value 对象（yaml:744-757）vs 实际返回 plain values（lib.rs:1674-1708）；
    - proto SECRET「按需解密」（proto:40）vs 实现注释「数据面不解密」（grpc.rs:58）。
11. **snapshot_required「起点被裁剪」路径未实现**：proto 承诺（proto:129-130），但服务器只对广播
    Lagged 发 snapshot_required（grpc.rs:255-266），从不检测 after_version 起点已被版本保留策略裁剪
    （grpc.rs:192-227、lib.rs:2837-2869）；SSE 通道连 Lagged 都直接丢流（dsh-watch/src/lib.rs:71-84）。
    断线超过保留窗口 → 中间事件静默丢失（严重 E-4）。

### B. SDK 代码质量

**TS**：浏览器兼容性声明不实（index.ts:1）——gRPC 通道依赖 grpc-js + proto-loader loadSync（fs 读
proto，grpc.ts:21-29），实质 Node-only；package.json exports 指向原始 .ts（6-8），无 dist/构建产物/
tsconfig，浏览器消费必须自备 TS bundler；@grpc/grpc-js 还是硬依赖。`ensureGrpc()` 动态 import 无
.catch（index.ts:83）；`request()` 无超时/AbortController（index.ts:111）——挂死端点永久 pending；
`watchHttp` 的 `const schedule` 引用先于声明（205-235，异步回调下恰好安全，写法脆弱）；
`listMembers` 返回裸 any[]（156-166）。

**Go**：**`http.Client{Timeout:5s}` 周期性掐断 SSE watch**（client.go:58,127）——HTTP watch 每 ~5 秒
必然断线重连，靠 after_version 续传兜底不丢数据，但连接持续抖动 + 重复投递（严重 E-2）。
**gRPC 方法全部忽略调用方 ctx**（grpc_client.go:45-51，context.Background）——Watch 卡在 Recv() 时
cancel 不生效，goroutine 泄漏（grpc-test/main.go:54-62 实测 cancel 后 goroutine 仍卡住）（严重 E-3）。
`valueFromProto` default 分支把未知类型当 secret 返回 "***"（70-72）；`request()` 非 200 不读 body（76-77）。

**Python**：urllib 无连接池/重试/自定义 CA；**`tls: bool = False` 参数是死代码**（config_client.py:57）；
`_request` 只捕 URLError/OSError（87），JSON 解析错误裸抛；gRPC stub 固定单地址（64-69），
gRPC 通道无端点池 failover。

**三语言共性**：watch 无事件去重（重放/重连重复回调：grpc.ts:137-140、grpc_client.go:164-166、
config_client.py:161）；snapshot_required 仅透传不自动重拉（可接受，测试零覆盖）。

### C. Admin UI（index.html，498 行）

**XSS 转义审计**：esc() 于 L349（& " < > ' 全覆盖）。**L254/L277-281 项目/分支名未转义**——当前被
服务器 `[a-z0-9-]` 强校验兜底（state.rs:47-66），不可利用，但 UI 安全完全依赖服务器校验而非转义；
**L333 onclick 内插值用 HTML 转义而非 JS 上下文转义**——`&#39;` 解码后进入 JS 字符串，若 group/key
含引号可突破；当前被 valid_key_name 封死。其余（L327-332/397-400/415-420/449-452/484-487）转义正确。
结论：转义总体全面，两处「依赖校验而非转义」+ 一处「上下文错误」隐患。

**CSP**：内联 script + onclick → 服务器必须放行 `script-src 'unsafe-inline'`（lib.rs:315-318）→
**该页面 CSP 形同虚设，XSS 即 RCE**（token 在 localStorage L195）。x-frame-options: DENY 在。

**状态管理缺陷**：
- **分支选择被重置**：`loadProject()` 无条件 `curBranch = bs[0].name`（L282）——发布/回滚/提升/删分支
  后跳回第一个分支（中危 UX bug，E-7）；
- **401 会话过期不跳登录页**（j() 只抛错，L201-210）；
- watch 用 EventSource 不带 after_version（L437），断线重连丢事件；events 面板无限追加（L438），内存增长。

**功能覆盖**：登录/登出/心跳/项目 CRUD/分支 CRUD/结构草稿+发布/值草稿编辑+发布/历史+回滚/对比/
promote/共享库草稿+发布+引用绑定/审计/SSE watch——与 openapi 基本齐。
**缺失**：secret reveal 查看（openapi 有 reveal=true+审计，UI 无入口）、集群成员/join/promote/remove、
admin force-logout/set-password/snapshot/rotate-master-key、项目管理员管理、共享解绑（bindRef 有、unbind 无）。

### D. 契约测试有效性

**优点**：三语言对同一 dev-single 跑同一断言集；「持续发布直到测试进程退出」消除订阅窗口竞态（思路正确）；
grpc 测试验证了 getItem 与 get 一致性。

**弱点**：
1. **断言强度弱**：get 只断言 host「存在」不校验值（test.ts:12、main.go:25-28、test.py:18-19）；
   watch 只断言 version 递增（test.ts:18、main.go:34-36、test.py:25），不校验 ty/changes/request_id/
   structure_version/snapshot_required。
2. **对拍覆盖面窄**：无跨语言结果比较；**ListMembers 从未真正断言**——dev-single 必然失败被
   try/catch 吞掉（grpc-test.ts:21、go/main.go:45-49、grpc-test.py:30-34），该 RPC 契约零有效测试。
3. **watch 时序**：每秒一发最多 60 发"霰弹枪"，任何后续事件都过——掩盖首事件遗漏/乱序/重复投递；
   不验证「订阅后首个事件即最新发布」。Go HTTP watch 5s 断线问题测不出来。
4. **无断线续传测试**：从不重启服务器/断 TCP——after_version 续传、snapshot_required 慢消费者、
   版本保留裁剪后续传全部零覆盖。
5. 脚本健壮性：BIN 默认路径指向 /home/alex/...（sdk-contract-test.sh:4、grpc 版:5）；`pkill -x dsh`
   （:10-11）杀伤面过大；grpc 版 `go mod tidy` 失败不报错（:58）；set -u 无 set -e。

### E. 新发现缺陷分级

**严重**：
1. **Admin UI 草稿编辑器破坏非 string 值**（index.html:332：int/float/json/array/secret 显示
   `[object Object]`；保存 `parseInt→NaN||0` 存成 0，:363,376）。**bool 草稿项 checkbox 恒不勾选**
   （:331 判断 v 对象而非 v.bool_value）→ 保存即写 false。数据丢失。
2. **Go SDK HTTP watch 每 5s 被 client 超时掐断**（client.go:58 + :127）。
3. **Go gRPC SDK 忽略调用方 ctx**（grpc_client.go:45-51）——Watch 无法取消、goroutine 泄漏。
4. **断线续传在版本被裁剪后静默丢事件**：服务器不检测起点被裁剪（proto:129-130 承诺未实现，
   grpc.rs:192-227、lib.rs:2837-2869）；SSE 连 Lagged 都丢流（dsh-watch/src/lib.rs:71-84）。
   SDK 无从得知缓存失效 → 配置变更静默丢失。

**中**：5. Python gRPC watch 退避恒 400ms；6. TS「浏览器/Node」声明不实；7. Admin UI 分支选择重置；
8. GetItem RPC 死代码；9. 契约文档三处矛盾（yaml:607/744-757、proto:40 vs 实现）；10. watch 无版本去重；
11. Admin UI 401 不引导重新登录。

**低**：12. TS get() HTTP version 参数忽略/Go 无该参数；13. TS request() 无超时；14. HTTP 4xx/5xx
不参与 failover、watch 全用 endpoints[0]；15. Python tls 死代码；16. onclick 转义上下文错误；
17. gRPC watch 重连均固定间隔；18. Python 空端点列表 IndexError；19. EventSource 不带 after_version、
events 面板无限增长；20. 契约脚本 BIN 路径/pkill 杀伤面。
