# Defing K3s 多节点集群部署方案

> 依据当前代码实现（`server/` 各 crate、`deploy/Dockerfile`、`scripts/cluster-demo.sh`、`dev_docs/research-k8s-k3s-integration.md`）规划的 K3s 部署方案。
> 交付物：本方案文档（含完整 manifests 与验证命令）；用户已确认：**仅方案文档**，且包含**通用写转发中间件**的代码改动建议（解决外部访问管理面的 428 问题）。
>
> **落地状态（2025-08 更新）**：Phase 0 写转发中间件已实现并通过全部测试（`dsh-api` 40 用例 + `cluster-demo.sh` e2e + `cargo test --workspace` 全绿）；manifests 已落盘 `deploy/k3s/`（`namespace/secret/entrypoint-configmap/headless-service/statefulset/public-service/ingress/pdb` + `README.md`）。§7.3 测试落地版做了健壮性增强：经 `/api/v1/cluster/members` 动态确认当前 leader/follower（`current_leader` 为 JSON 数字需 `as_u64`），避免并发 seed 建群初期 leader 迁移导致的竞态；其余与下文一致。

---

## 0. Plan Header（写作计划头）

- **Goal**：在 K3s 上以 StatefulSet 部署 Defing 3 节点 Raft 集群，数据面/管理面对外可用，具备安全基线、持久化、PDB、优雅升级、扩容/缩容与备份恢复能力。
- **Architecture**：单二进制 `defing` × 3（StatefulSet，`--bootstrap-peers` 静态成员表建群，全员 voter）+ Headless Service（raft/http 内部互访）+ ClusterIP/LoadBalancer Service（数据面 + 管理面对外）+ Secret（主密钥/集群令牌/管理员密码）+ PDB（minAvailable 2）+ 反亲和 + `local-path` PVC；前置一个最小代码改动（写转发中间件）使外部管理面写操作完整可用。
- **Tech Stack**：K3s（v1.28+，任意数据存储后端）、Rust `defing` 二进制、`deploy/Dockerfile` 镜像、`local-path` StorageClass、可选用 K3s 内置 Traefik Ingress / ServiceLB。
- **Baseline/Authority Refs**：
  - 需求/背景：`dev_docs/research-k8s-k3s-integration.md`（§3 Helm/StatefulSet 最佳实践、§4 K3s 差异、§7 路线图）
  - 集群行为契约：`dev_docs/research-cluster-bootstrap.md`、`dev_docs/defing-cluster.md`（坑 C1–C4）、`README.md`（集群启动）
  - 代码事实：`server/crates/dsh-cli/src/main.rs`（CLI/seed 校验/join）、`server/crates/dsh-api/src/lib.rs`（路由/428/转发先例）、`server/crates/dsh-crypto/src/lib.rs`（主密钥/ring）
- **Compatibility Boundary**：
  - 不改动既有 CLI 参数语义；`--bootstrap-peers` 校验规则（本节点条目必须与 `--http-addr`/`--raft-addr` 字符串一致、拒绝 `0.0.0.0`）是网络模型设计的硬约束。
  - 中间件仅新增行为：对"写请求命中 follower 返回的 428 ERR_LEADER_REDIRECT"做服务端转发；登录/主密钥轮换/cluster-join 保持既有行为不变（豁免清单见 §7）。
  - 方案不包含：Helm chart 打包（后续工作）、K3s 控制面高可用（`--cluster-init` 等，与 Defing 无关）、`read_mode=linear` 下外部读的 428 处理（默认 `stale` 无此问题）。
- **TDD Route**：
  - Mode: `off`
  - Decision: `skipped`（唯一代码改动为写转发中间件；按"最小改动 + 回归验证"执行，无显式 strict 要求）
  - Strict authority: not applicable
  - Test posture: post-change regression（新增集成测试 `leader_write_forward` + 既有 `cluster-demo.sh`/`api-surface-test.sh` 回归）
  - Reason: 用户未要求 TDD；中间件行为可由集群集成测试直接验证
  - Verification: `cargo test -p dsh-api --test leader_write_forward`；`bash ../scripts/cluster-demo.sh`
- **Verification**：见 §10（部署验证清单，命令级）。

---

## 1. 结论摘要（TL;DR）

1. **部署形态**：`Namespace defing` 内 1 个 StatefulSet（3 副本 `defing-0..2`，对应 `--node-id 1..3`）+ 2 个 Service（Headless `defing` 供 raft/内部互访；`defing-public` 供数据面/管理面对外负载均衡）+ Secret + PDB + ConfigMap 启动脚本。K3s 零特殊适配：默认 `local-path` 即存储，ServiceLB/Traefik 即入口。
2. **建群方式**：沿用 README 推荐的 `--bootstrap-peers` 静态成员表（全员 voter、无需 join/promote），幂等 resume 设计天然适配 K8s 静态启动命令——节点重启/漂移后自动恢复，seed 与成员表不一致仅 WARN（不覆盖）。
3. **网络模型（本方案核心决策）**：
   - raft/http 监听地址绑 **pod 短主机名**（`defing-0`，来自 `/etc/hosts`，免 DNS 启动竞争）；成员表内地址同样用短主机名（同 namespace 经 DNS search domain 互相可解析）。
   - gRPC 地址是硬约束：`--grpc-addr` **必须 IP 字面量**（`0.0.0.0:8383`），且 gRPC 不落成员表，无校验冲突。
   - 数据面（读/watch）任何节点可服务（`stale` 本地读 + watch 经 raft 转发广播），Service 负载均衡即可。
   - 管理面写：非 leader 节点返回 428 + leader_hint（pod DNS）；**前置最小代码改动**（§7 写转发中间件）使其服务端透明转发到 leader，外部客户端无感。
4. **安全**：主密钥经 Secret 注入并落盘 PVC（启用 API 轮换，ring 持久化）；join/raft 令牌共享一个强随机 Secret；管理员密码入 Secret；容器非 root（uid 10001）+ `fsGroup`；可选 `readOnlyRootFilesystem` 加固。
5. **运维**：PDB `minAvailable: 2` 保证升级/驱逐不丢 quorum；扩容走 `--join`（ordinal ≥ 初始成员数的 Pod 自动 join）；缩容先 `cluster/remove` 再 `scale`；节点/盘丢失后删 PVC 重建即经 Raft 追平恢复。
6. **工作量**：Phase 0（中间件，~1 个文件 + 1 个测试）M；Phase 1–4（镜像 + manifests + 部署 + 验证）S。

---

## 2. 代码基线盘点（与 K3s 部署直接相关的事实）

| # | 事实 | 来源 | 对 K3s 方案的影响 |
|---|---|---|---|
| F1 | 集群模式强制 `--node-id`、`--data-dir`、`--join-token`、`--raft-token`（三节点须相同令牌）；缺一拒绝启动 | `dsh-cli/src/main.rs:847-859` | Secret 注入三个参数 |
| F2 | `--bootstrap-peers` 三段式 `node_id@raft_addr@http_addr`；首次建群全员 voter；有持久化状态自动 resume（忽略 seed，不一致仅 WARN） | `main.rs:906-943` | 静态启动命令幂等；K8s 重启/换 Pod 安全 |
| F3 | seed 校验：本节点条目 `raft_addr`/`http_addr` 必须与 `--raft-addr`/`--http-addr` **字符串完全一致**；条目拒绝 `0.0.0.0`/`::`；raft/http 地址各自不得重复 | `main.rs:400-520` | 监听地址与成员表必须同一字符串（短主机名方案 §5.1） |
| F4 | `--http-addr`/`--raft-addr` 经 `TcpListener::bind`（`ToSocketAddrs`，可绑主机名）；`--grpc-addr` 必须 `parse::<SocketAddr>`（**IP 字面量**） | `main.rs:1037,1069,1087` | gRPC 只能绑 `0.0.0.0`；http/raft 可绑短主机名 |
| F5 | grpc 地址不落成员表（seed/join 均写 `grpc_addr: ""`）；`listMembers` 返回的 grpc_addr 为空 | `main.rs:456`、`dsh-api/src/lib.rs:3211`、`grpc.rs:403` | SDK 端点池由客户端配置；gRPC 只做 Service 暴露 |
| F6 | 写路径非 leader → `ErrorKind::LeaderRedirect` → HTTP **428** + `detail.leader_hint`（= leader 的 `http_addr`）；仅 login / rotate-master-key 做服务端转发；cluster/join 的 428 由客户端（join_cluster）跟随 | `dsh-api/src/lib.rs:224-238,2697-2740,3404-3430`、`main.rs:327-392` | **管理面写经 LB 命中 follower 会 428** → 前置中间件（§7） |
| F7 | 数据面读默认 `stale`（本地直读，无 leader 依赖）；watch（SSE/gRPC）由 raft apply 转发到每节点 hub，任意节点可订阅 | `dsh-api/src/lib.rs:147-174`、`main.rs:887` | 数据面对外负载均衡无脑可用 |
| F8 | 主密钥：`DSH_MASTER_KEY`（base64 32B env）优先，否则 `--master-key-file`（raw 32B 文件）；ring 文件路径 = `{key_file}.ring.json`（**必须可写**）；API 轮换需要 `--master-key-file`（ring 持久化），纯 env 时拒绝 | `dsh-crypto/src/lib.rs:236-272,297`、`dsh-api/src/lib.rs:3363-3368` | key 文件必须落在**可写 PVC**（§6.2），不能只读挂 Secret |
| F9 | 容器：非 root（uid 10001）+ `su-exec` 降权 entrypoint（root 启动时 chown `/data`）；暴露 8383/8384/8385；镜像无 curl/wget（探测用 K8s `httpGet`，kubelet 发起，无需容器内客户端） | `deploy/Dockerfile`、`deploy/docker-entrypoint.sh` | probes 用 `httpGet`；entrypoint 兼容 root/非 root |
| F10 | `/healthz`（存活）、`/readyz`（raft 有日志才 200，否则 503）、`/metrics`（Prometheus 文本） | `dsh-api/src/lib.rs:3085-3095,4011-4013` | liveness=/healthz，readiness=/readyz |
| F11 | `--join` 客户端自动跟随 428 leader_hint（30s 超时重试） | `main.rs:360-384` | 扩容节点 `--join http://defing-0:8384` 即可，不依赖 leader 身份 |
| F12 | `read_mode=linear` 时 follower 读也返回 428（本方案默认 `stale`，不涉及） | `dsh-api/src/lib.rs:147-174` | 方案范围外，见 §13 |
| F13 | 容器启动命令幂等化是设计目标（"由此 compose/k8s 可用静态启动命令，无需 shell 判断数据目录"） | `main.rs:875-881` | 与 StatefulSet 静态 command 完美契合 |

---

## 3. 目标架构

```
                          ┌────────────────────── K3s 集群 ──────────────────────┐
  外部客户端                │  Namespace: defing                                  │
 ┌─────────┐               │                                                     │
 │ Admin UI│──HTTP/HTTPS──▶│  [Traefik Ingress(可选)] → ServiceLB/NodePort        │
 │ 浏览器   │               │        │                                            │
 │ SDK Pods│──gRPC/HTTP───▶│        ▼                                            │
 │ (同集群) │               │  Service: defing-public (ClusterIP/LoadBalancer)    │
 └─────────┘               │    ports: 8384 HTTP(admin+数据面) / 8383 gRPC        │
                           │        │ 负载均衡（读/watch 任意节点可用；             │
                           │        │ 写命中 follower 由中间件转发到 leader）       │
                           │        ▼                                            │
                           │  StatefulSet: defing (replicas=3)                   │
                           │  serviceName: defing (Headless, clusterIP: None)    │
                           │    defing-0 ──node 1──┐  raft RPC :8385             │
                           │    defing-1 ──node 2──┼── 短主机名互访（DNS search）  │
                           │    defing-2 ──node 3──┘  http   :8384               │
                           │    每 Pod: PVC data-defing-<n> (local-path)         │
                           │    PDB minAvailable=2 · 反亲和 · 优雅终止 60s        │
                           │    Secret: master.key/cluster-token/admin-password  │
                           └─────────────────────────────────────────────────────┘
```

**清单总览**（§9 给出完整 YAML）：

| 资源 | 名称 | 作用 |
|---|---|---|
| Namespace | `defing` | 命名空间隔离 |
| Secret | `defing-secrets` | 主密钥（raw 32B）、`cluster-token`（join+raft 共用）、`admin-password` |
| ConfigMap | `defing-entrypoint` | 按 ordinal 生成 seed / 选择 bootstrap 或 join 的启动脚本 |
| Service（Headless） | `defing` | 稳定 DNS（`defing-0..2`），raft/http/gRPC 内部互访 |
| Service | `defing-public` | 数据面 + 管理面对外（ClusterIP，可升 LoadBalancer/NodePort） |
| StatefulSet | `defing` | 3 副本，volumeClaimTemplates + 探针 + 反亲和 + 安全上下文 |
| PodDisruptionBudget | `defing-quorum` | `minAvailable: 2` |
| Ingress（可选） | `defing` | Traefik 域名入口 + TLS（HTTP 管理面/数据面；gRPC 走 LB 或 Traefik h2c） |

---

## 4. 需求与范围核对

```text
Requirement Ready Check:
- Requirement source refs: 用户请求（K3s 多节点集群部署方案）；用户确认（仅方案文档；含写转发中间件建议）
- Goals and scope refs: research-k8s-k3s-integration.md §3/§4/§7
- Acceptance / verification criteria refs: §10 验证清单（集群就绪、跨节点读写、中间件转发、容错、扩容）
- Open blocker questions: 无
- Decision: ready

BaselineUsageDraft:
- Required baseline refs: research-k8s-k3s-integration.md、research-cluster-bootstrap.md、defing-cluster.md、README.md
- Delivered context refs: 上述文档 + dsh-cli/dsh-api/dsh-crypto 源码（本方案 §2 已逐条核对）
- Cited in plan refs: §2 表格（F1–F13）
- Missing refs: 无
- Decision: continue
```

---

## 5. 网络模型设计

### 5.1 监听地址 vs 成员表地址（硬约束推导）

- seed 校验（F3）要求本节点条目与 `--raft-addr`/`--http-addr` **字符串一致**，且 seed 拒绝 `0.0.0.0`。
- 因此监听地址**不能**用 `0.0.0.0`，必须用本 Pod 可路由且可绑定的名字。
- 选择：**pod 短主机名**（`defing-0`）：
  - 本 Pod 绑定：`/etc/hosts` 含 `defing-0` → `TcpListener::bind("defing-0:8384")` 绑到 Pod IP，**不依赖 DNS 启动时序**；
  - 其他 Pod 访问：同 namespace 下 `defing-0` 经 resolv.conf search domain（`<ns>.svc.cluster.local ...`）解析到 `defing-0.defing.<ns>.svc.cluster.local`。
  - 成员表/重定向/join 跟随全部使用短主机名，集群内一律可解析。
- gRPC：F4 要求 IP 字面量 → `--grpc-addr 0.0.0.0:8383`（gRPC 不落成员表（F5），无校验冲突；Service 按 Pod IP 路由不受影响）。

### 5.2 服务分层与流量模型

| 流量 | 入口 | 路径 | 说明 |
|---|---|---|---|
| raft RPC（:8385） | 仅集群内部 | Pod→Pod（短主机名） | 不进任何 Service 转发面；Headless 仅提供 DNS |
| 数据面 HTTP/SSE（:8384） | `defing-public` → 任意 Pod | `/v1/...` snapshot/config/watch | `stale` 本地读 + watch 任意节点（F7）；LB 安全 |
| 数据面 gRPC（:8383） | `defing-public` → 任意 Pod | ConfigService | 同左 |
| 管理面 HTTP（:8384） | `defing-public` / Ingress → 任意 Pod | `/admin`、`/api/v1/**` | 读任意节点；**写经中间件转发到 leader**（§7） |
| 监控 | `defing-public` | `/healthz` `/readyz` `/metrics` | 探针 + Prometheus 抓取 |

### 5.3 leader 重定向（428）影响面与处理

- 读（GET，`stale` 默认）：任何节点直接服务，无 428。
- 写（POST/PUT/PATCH/DELETE）：命中 follower → 428 + leader_hint（pod DNS）。**外部客户端无法解析 pod DNS 且无跟随实现**（Admin UI 无 428 处理）→ 这是"管理面对外可用"的唯一阻断点。
- 处理：§7 中间件在服务端把 428 透明转发到 leader（复用 login/rotate 既有转发模式），外部客户端无感；`cluster/join` 的 428 保持"客户端跟随"契约（F11，扩容依赖）。

---

## 6. 安全设计

### 6.1 Secret 清单（一次性生成，`kubectl -n defing create secret` 或 apply）

| 键 | 内容 | 用途 |
|---|---|---|
| `master.key` | raw 32B（非 base64 文本；Secret `data` 里为 base64 编码） | `--master-key-file`；`defing --gen-master-key` 输出 base64 → `base64 -d` 得 raw |
| `cluster-token` | 强随机 ≥32 字符 | `--join-token` 与 `--raft-token` 共用（README/compose 同款约定） |
| `admin-password` | 强密码 ≥12 位 | `--admin-password` |

生成命令：

```bash
kubectl create namespace defing
kubectl -n defing create secret generic defing-secrets \
  --from-literal=admin-password='<强密码>' \
  --from-literal=cluster-token="$(openssl rand -hex 32)" \
  --from-file=master.key=<(defing --gen-master-key | base64 -d)   # raw 32B
# 或：openssl rand -base64 32 | base64 -d > master.key
```

### 6.2 主密钥落盘规则（F8 推导）

- **禁止**只读挂载 Secret 到 `/etc/defing/master.key` 直接当 `--master-key-file`：ring 文件（`{key_file}.ring.json`）会尝试写在只读卷 → 轮换失败/启动异常。
- **推荐**：entrypoint 启动时把 Secret 挂载的 key 复制到 PVC 数据目录（`/data/master.key`），`--master-key-file /data/master.key` → ring 落 PVC、可写、重启持久；API 轮换可用（raft 复制 + 各节点 `save_ring`）。
- **简化替代**：`DSH_MASTER_KEY` env（`data: DSH_MASTER_KEY: <base64 32B>`）→ 无 ring、**API 轮换被拒**（`dsh-api:3363`），适合不需要在线轮换的场景。

### 6.3 容器安全上下文

- Pod `fsGroup: 10001`（kubelet 将 PVC 组归属设为 10001，uid 10001 可写）；
- 容器 `runAsUser: 10001, runAsGroup: 10001, runAsNonRoot: true`（`docker-entrypoint.sh` 检测非 root 直接 exec，跳过 chown）；
- 加固项（默认开启，若发现落盘异常再关闭）：`readOnlyRootFilesystem: true`、`allowPrivilegeEscalation: false`、`capabilities.drop: ["ALL"]`、`seccompProfile: {type: RuntimeDefault}`。
- 说明：defing 运行期只写 `/data`（redb + ring + key 副本），日志走 stdout，rootfs 只读可行。

### 6.4 其他

- 入口代理后建议 `--trusted-proxy` 指向 Ingress 网段（K3s 默认 flannel `10.42.0.0/16`），保证登录节流的 XFF 可信（F4 登录节流）。
- 数据面每项目访问令牌在 Admin UI 创建（SHA-256 落盘），与集群部署正交，无需额外配置。
- 生产**不传** `--allow-no-master-key`（主密钥必配）。

---

## 7. Phase 0 —— 前置代码任务：通用写转发中间件

```text
Change Necessity:
- User-visible need: 外部客户端（浏览器 Admin UI / curl 脚本）经 LB/Ingress 访问管理面时，写操作不再约 2/3 概率返回 428（命中 follower 时）
- No-change / non-code option: 管理面仅集群内访问（kubectl port-forward / 跳板）——外部管理面写不可用，与"管理面对外可用"目标冲突
- Why code change is necessary: 428 的 leader_hint 是 pod DNS（`defing-1:8384`），外部客户端不可解析；服务端转发是唯一无需改造任何客户端的方案，且与既有 login/rotate 转发模式同构
- Minimum change boundary: `server/crates/dsh-api/src/lib.rs`（1 个中间件 + build_router 挂载）+ 1 个集成测试文件
- Decision: code-change
```

### 7.1 改动点 A：`server/crates/dsh-api/src/lib.rs` 新增中间件

在 `build_router`（约 4009 行）之前、模块内新增（完整代码，可直接落地）：

```rust
/// 通用写转发（K3s/Service 负载均衡场景）：写请求落到 follower 时，服务端把
/// 428 ERR_LEADER_REDIRECT 透明转发到 leader 的同一路径，客户端无感。
///
/// 设计要点：
/// - 仅 POST/PUT/PATCH/DELETE 且路径以 /api/v1/ 开头才可能转发；
/// - 豁免：/api/v1/login、/api/v1/admin/rotate-master-key（已有内联服务端转发）、
///   /api/v1/cluster/join（428 是「客户端跟随 leader_hint」的设计契约，F11）；
/// - 仅当响应为 428 且 body.detail.leader_hint 非空时转发；单次尝试，
///   leader 不可达则回落原 428（客户端行为不变）；
/// - 转发保留原请求头（Authorization 等；会话令牌经 Raft 复制，全集群有效）。
async fn forward_leader_writes(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, Method, StatusCode};

    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let is_write = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let eligible = is_write
        && path.starts_with("/api/v1/")
        && path != "/api/v1/login"
        && path != "/api/v1/admin/rotate-master-key"
        && path != "/api/v1/cluster/join";
    if !eligible {
        return next.run(req).await;
    }

    // 缓冲请求体（重建交给 handler；转发时复用同一份 bytes）
    let (parts, body) = req.into_parts();
    let req_headers = parts.headers.clone();
    let body_bytes = match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return next
                .run(axum::extract::Request::from_parts(
                    parts,
                    axum::body::Body::empty(),
                ))
                .await
        }
    };
    let req = axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(body_bytes.clone()),
    );
    let resp = next.run(req).await;
    if resp.status() != StatusCode::PRECONDITION_REQUIRED {
        return resp;
    }

    // 解析 428 体 → leader_hint
    let (rparts, rbody) = resp.into_parts();
    let rbytes = match axum::body::to_bytes(rbody, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return axum::response::Response::from_parts(rparts, axum::body::Body::empty())
        }
    };
    let hint: Option<String> = serde_json::from_slice::<serde_json::Value>(&rbytes)
        .ok()
        .and_then(|v| {
            v.get("detail")
                .and_then(|d| d.get("leader_hint"))
                .and_then(|h| h.as_str())
                .map(String::from)
        })
        .filter(|h| !h.is_empty());
    let Some(hint) = hint else {
        // 非 LeaderRedirect 的 428（如 join 契约）：原样返回
        return axum::response::Response::from_parts(rparts, axum::body::Body::from(rbytes));
    };

    // 转发到 leader（http_addr 无 scheme → 补 http://）
    let base = if hint.starts_with("http://") || hint.starts_with("https://") {
        hint
    } else {
        format!("http://{hint}")
    };
    let query = uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{base}{path}{query}");

    let client = reqwest::Client::new();
    let fwd = client
        .request(method, &target)
        .headers(req_headers)
        .body(body_bytes)
        .timeout(std::time::Duration::from_secs(10));
    match fwd.send().await {
        Ok(leader_resp) => {
            let status = leader_resp.status();
            let lheaders = leader_resp.headers().clone();
            let lbytes = leader_resp.bytes().await.unwrap_or_default();
            let mut builder = axum::response::Response::builder().status(status);
            for (k, v) in lheaders.iter() {
                if k != header::CONTENT_LENGTH && k != header::TRANSFER_ENCODING {
                    builder = builder.header(k, v);
                }
            }
            builder
                .header("X-Defing-Forwarded-To", &target)
                .body(axum::body::Body::from(lbytes))
                .unwrap_or_else(|_| axum::response::Response::new(axum::body::Body::empty()))
        }
        Err(_) => {
            // leader 不可达：回落原 428（客户端行为不变，可自行重试）
            axum::response::Response::from_parts(rparts, axum::body::Body::from(rbytes))
        }
    }
}
```

### 7.2 改动点 B：`build_router` 挂载（`dsh-api/src/lib.rs:4117-4123`）

在 `security_headers` 之后、`count_http` 之前插入一行（注册顺序靠后者在外层；此处使转发包住鉴权，且 `count_http` 仍统计首段请求）：

```rust
    router = router.layer(axum::middleware::from_fn(security_headers));
    // 写转发：包住鉴权+handler，仅对鉴权通过的写响应处理 428
    router = router.layer(axum::middleware::from_fn(forward_leader_writes));
    // G5/D32：HTTP 计数最外层（统计全部请求含 healthz/metrics）
    router = router.layer(axum::middleware::from_fn(count_http));
```

### 7.3 改动点 C：新增集成测试 `server/crates/dsh-api/tests/leader_write_forward.rs`

复用两处既有装配模式：3 节点集群骨架取自 `server/crates/dsh-raft/tests/cluster.rs`（`NetworkFactory` + `register` + `initialize_cluster`）；单节点 HTTP 路由装配取自 `server/crates/dsh-api/tests/cluster_join_idempotent.rs` 的 `start()`（`build_router` + 真实监听）。

**关键点**：中间件按 428 响应体里的 `leader_hint`（= leader 的 `NodeInfo.http_addr`）转发，因此每个节点的 HTTP 监听地址必须**等于其 `NodeInfo.http_addr`**——装配顺序改为：先绑 `127.0.0.1:0` 拿真实地址 → 用该地址构造 `NodeInfo` → 建 raft 节点 → 建群 → 再 `axum::serve`。完整实现：

```rust
//! 写转发中间件契约：
//!   - 写请求命中 follower → 服务端转发到 leader → 200（核心断言）；
//!   - /api/v1/cluster/join 的 428 不被转发（客户端跟随契约保留，F11）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use dsh_api::{build_router, ApiState};
use dsh_core::StateMachine;
use dsh_raft::*;
use dsh_storage::RedbStorage;
use dsh_watch::WatchHub;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TestNode {
    base: String,
    node_id: u64,
    is_leader: bool,
}

/// 3 节点真实 raft 集群（seed 建群，全员 voter）+ 每节点完整 HTTP 路由。
/// 每个节点的 HTTP 监听地址即其 NodeInfo.http_addr（428 hint 必须可直达）。
async fn start3() -> Vec<TestNode> {
    let network = NetworkFactory::new();
    let mut rafts: Vec<RaftHandle> = Vec::new();
    let mut seed: BTreeMap<NodeId, NodeInfo> = BTreeMap::new();
    // 先绑 HTTP 端口（ephemeral）拿真实地址，再建 raft 节点
    let mut pending = Vec::new();
    for id in 1..=3u64 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = listener.local_addr().unwrap().to_string();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dsh-fwd-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = RedbStorage::open(&dir.display().to_string()).unwrap();
        let db = storage.raw_db();
        let sm = Arc::new(RwLock::new(StateMachine::new(Box::new(storage))));
        let sm_store = Arc::new(StateMachineStore::new(sm.clone(), db.clone()));
        let log_store = LogStore::new(db.clone());
        let info = NodeInfo {
            grpc_addr: format!("127.0.0.1:{}", 18000 + id),
            http_addr, // ← 与真实监听地址一致，428 leader_hint 可直达
            raft_addr: format!("127.0.0.1:{}", 17000 + id),
        };
        seed.insert(id, info.clone());
        let raft = new_raft_node(id, info.clone(), log_store, sm_store, &network, dev_config())
            .await
            .unwrap();
        network.register(id, raft.clone());
        rafts.push(raft.clone());
        pending.push((id, listener, sm, raft));
    }
    // seed 建群（与 README 推荐路径一致；所有节点传相同 map）
    for raft in &rafts {
        initialize_cluster(raft, seed.clone()).await.unwrap();
    }
    // 等待 leader 产生
    let mut leader_id = None;
    for _ in 0..100 {
        for r in &rafts {
            if let Some(l) = r.current_leader().await {
                leader_id = Some(l);
                break;
            }
        }
        if leader_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let leader_id = leader_id.expect("cluster should elect a leader");

    // 每节点挂 HTTP（监听地址 = NodeInfo.http_addr）
    let mut nodes = Vec::new();
    for (id, listener, sm, raft) in pending {
        let state = ApiState::new(
            sm,
            WatchHub::new(),
            Some(raft),
            Some(id),
            None,
            Duration::from_secs(86400),
            "admin-pw".into(),
            None,
        );
        let app = build_router(state);
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        nodes.push(TestNode {
            base: format!("http://{addr}"),
            node_id: id,
            is_leader: id == leader_id,
        });
    }
    nodes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_through_follower_is_forwarded() {
    let nodes = start3().await;
    let leader = nodes.iter().find(|n| n.is_leader).expect("leader node");
    let follower = nodes.iter().find(|n| !n.is_leader).expect("follower node");

    // 1) 经 follower 登录（login 已有内联转发，验证集群会话可用）
    let login = reqwest::Client::new()
        .post(format!("{}/api/v1/login", follower.base))
        .json(&serde_json::json!({ "password": "admin-pw" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status().as_u16(), 200, "login via follower should forward");
    let token: String = login
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // 2) 经 follower 写项目 → 中间件转发到 leader → 200（核心断言）
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/projects", follower.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "svc-a" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "follower 写应被转发成功: {:?}",
        resp.text().await
    );
    assert!(resp.headers().contains_key("x-defing-forwarded-to"));

    // 3) 任一节点可读到（复制生效）
    let list = reqwest::Client::new()
        .get(format!("{}/api/v1/projects", leader.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = list.json().await.unwrap();
    assert!(
        body.as_array().unwrap().iter().any(|p| p["id"] == "svc-a"),
        "project should be replicated: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_428_is_not_forwarded() {
    // 反例：/api/v1/cluster/join 的 428 保持「客户端跟随」契约（F11），不被中间件转发
    let nodes = start3().await;
    let follower = nodes.iter().find(|n| !n.is_leader).expect("follower node");
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/cluster/join", follower.base))
        .json(&serde_json::json!({
            "node_id": 9,
            "http_addr": "127.0.0.1:9009",
            "raft_addr": "127.0.0.1:7009",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 428, "join 428 不应被转发");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "ERR_LEADER_REDIRECT");
    assert!(body["detail"]["leader_hint"].is_string());
}
```

### 7.4 验证（Phase 0 完成标准）

```bash
cd server && source ../scripts/build-env.sh
cargo test -p dsh-api --test leader_write_forward          # 新增：follower 写 → 200
cargo test -p dsh-api                                      # 既有 API 测试全绿（428/转发豁免回归）
bash ../scripts/cluster-demo.sh                            # 既有集群 e2e（对 leader 写不受影响）
bash ../scripts/api-surface-test.sh                        # API 面回归（可选）
```

**回归关注点**：`/api/v1/cluster/join` 仍返回 428（client-follow，F11）；`/api/v1/login`、rotate-master-key 不重复转发（豁免清单）；leader 上直接写不产生任何转发（无 428 则原样返回）。

---

## 8. Phase 1 —— 镜像构建与推送

```bash
# 构建（构建上下文必须是仓库根目录：Dockerfile 内 COPY server/ ./server/）
docker build -f deploy/Dockerfile -t <registry>/defing:v0.1.0 .

# 多架构（K3s 节点可能 arm64，如树莓派）：
docker buildx build --platform linux/amd64,linux/arm64 \
  -f deploy/Dockerfile -t <registry>/defing:v0.1.0 --push .

# 私有 registry：K3s 侧配置 containerd 镜像仓库认证（/etc/rancher/k3s/registries.yaml）
# 或 Pod 侧 imagePullSecrets（manifest 已预留字段）。
```

镜像要点（与既有 `deploy/Dockerfile` 一致）：builder 为 `rust:1.97` + protoc（dsh-api build.rs 需要）；运行时 `debian:bookworm-slim` + `su-exec`；`ENTRYPOINT ["/docker-entrypoint.sh"]`（root 启动 chown `/data` 后降权，或非 root 直接 exec）。

---

## 9. Phase 2 —— K3s manifests（完整清单）

### 9.1 `namespace.yaml`

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: defing
```

### 9.2 `secret.yaml`（占位值须替换；推荐 `kubectl create secret` 而非入库）

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: defing-secrets
  namespace: defing
type: Opaque
stringData:
  # --admin-password：强密码（≥12 位）
  admin-password: "<ADMIN_PASSWORD>"
  # --join-token 与 --raft-token 共用（集群内所有节点必须相同）
  cluster-token: "<CLUSTER_TOKEN_≥32字符>"
data:
  # raw 32B 主密钥的 base64（openssl rand -base64 32 | base64 -d > master.key; base64 -w0 master.key）
  # 注意：Secret 里存 base64(32B raw)，挂载后文件是 raw 32B（--master-key-file 期望格式）
  master.key: "<BASE64_OF_RAW_32B>"
```

### 9.3 `entrypoint-configmap.yaml`（启动脚本：按 ordinal 生成 seed / 选择 bootstrap vs join）

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: defing-entrypoint
  namespace: defing
data:
  k3s-entrypoint.sh: |
    #!/bin/sh
    # Defing K3s 启动脚本（以 defing 用户运行；命令经 docker-entrypoint.sh 降权后执行）
    set -e
    # StatefulSet ordinal → node_id（defing-0 → 1）
    ORDINAL=$(printf '%s' "$HOSTNAME" | sed 's/.*-\([0-9]*\)$/\1/')
    NODE_ID=$((ORDINAL + 1))
    # 初始成员表节点数：ordinal < SEED_NODES 用 --bootstrap-peers 建群；其余用 --join 扩容
    SEED_NODES=${SEED_NODES:-3}
    HTTP_PORT=${HTTP_PORT:-8384}
    RAFT_PORT=${RAFT_PORT:-8385}
    GRPC_PORT=${GRPC_PORT:-8383}
    DATA_DIR=${DATA_DIR:-/data}
    # 主密钥落盘 PVC（ring 可写、持久 → 在线轮换可用；F8）
    KEY_FILE="$DATA_DIR/master.key"
    cp /etc/defing/master.key "$KEY_FILE" && chmod 600 "$KEY_FILE"
    CLUSTER_TOKEN=$(cat /etc/defing/cluster-token)
    ADMIN_PASSWORD=$(cat /etc/defing/admin-password)

    COMMON="--node-id $NODE_ID --data-dir $DATA_DIR --http-addr ${HOSTNAME}:${HTTP_PORT} \
      --raft-addr ${HOSTNAME}:${RAFT_PORT} --grpc-addr 0.0.0.0:${GRPC_PORT} \
      --admin-password $ADMIN_PASSWORD --master-key-file $KEY_FILE \
      --join-token $CLUSTER_TOKEN --raft-token $CLUSTER_TOKEN"

    if [ "$ORDINAL" -lt "$SEED_NODES" ]; then
      # 静态成员表建群：seed 条目用短主机名（本节点条目必须与 --http-addr/--raft-addr 一致，F3）
      SEED=""
      i=0
      while [ "$i" -lt "$SEED_NODES" ]; do
        [ -n "$SEED" ] && SEED="$SEED,"
        SEED="${SEED}$((i + 1))@defing-${i}:${RAFT_PORT}@defing-${i}:${HTTP_PORT}"
        i=$((i + 1))
      done
      exec defing $COMMON --bootstrap-peers "$SEED"
    else
      # 扩容节点：join 现有集群（自动跟随 428 leader_hint，F11）
      exec defing $COMMON --join "http://defing-0:${HTTP_PORT}"
    fi
```

> 说明：`$COMMON` 内嵌含密码/令牌，仅在容器内以 shell 变量传递，进程参数可见性属容器自身（与 compose 同款做法）；若需隐藏进程参数可改为读文件路径（`--admin-password` 不支持文件，保持现状）。

### 9.4 `headless-service.yaml`（StatefulSet 稳定 DNS）

```yaml
apiVersion: v1
kind: Service
metadata:
  name: defing
  namespace: defing
  labels: { app: defing }
spec:
  clusterIP: None          # Headless：仅为 Pod 提供稳定 DNS（defing-0..2）
  selector: { app: defing }
  ports:
    - { name: http,  port: 8384, targetPort: http }
    - { name: grpc,  port: 8383, targetPort: grpc }
    - { name: raft,  port: 8385, targetPort: raft }
```

### 9.5 `statefulset.yaml`

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: defing
  namespace: defing
  labels: { app: defing }
spec:
  serviceName: defing
  replicas: 3
  podManagementPolicy: OrderedReady
  updateStrategy:
    type: RollingUpdate            # 保守可选 OnDelete（手动逐个删 Pod 滚动）
  selector:
    matchLabels: { app: defing }
  template:
    metadata:
      labels: { app: defing }
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "8384"
    spec:
      terminationGracePeriodSeconds: 60   # Raft 优雅落盘窗口
      affinity:
        podAntiAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
            - weight: 100
              podAffinityTerm:
                topologyKey: kubernetes.io/hostname
                labelSelector:
                  matchLabels: { app: defing }
      securityContext:
        fsGroup: 10001              # PVC 组归属 → defing(uid 10001) 可写 /data
      containers:
        - name: defing
          image: <registry>/defing:v0.1.0
          imagePullPolicy: IfNotPresent
          # 镜像 ENTRYPOINT=/docker-entrypoint.sh（root 时 chown /data 并降权；非 root 直接 exec）
          command: ["/docker-entrypoint.sh"]
          args: ["sh", "/scripts/k3s-entrypoint.sh"]   # ConfigMap 文件无执行位，经 sh 调用
          ports:
            - { name: http, containerPort: 8384 }
            - { name: grpc, containerPort: 8383 }
            - { name: raft, containerPort: 8385 }
          env:
            - name: SEED_NODES
              value: "3"            # 初始成员数；扩容时保持 3，新增 Pod 自动 --join
          securityContext:
            runAsUser: 10001
            runAsGroup: 10001
            runAsNonRoot: true
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: { drop: ["ALL"] }
            seccompProfile: { type: RuntimeDefault }
          livenessProbe:
            httpGet: { path: /healthz, port: http }
            initialDelaySeconds: 15
            periodSeconds: 10
            timeoutSeconds: 3
            failureThreshold: 6
          readinessProbe:
            httpGet: { path: /readyz, port: http }   # raft 有日志才 200（F10）
            initialDelaySeconds: 10
            periodSeconds: 5
            timeoutSeconds: 3
          resources:
            requests: { cpu: 100m, memory: 256Mi }
            limits:   { cpu: "1",  memory: 1Gi }
          volumeMounts:
            - { name: data, mountPath: /data }
            - { name: secrets, mountPath: /etc/defing, readOnly: true }
            - { name: entrypoint, mountPath: /scripts, readOnly: true }
      volumes:
        - name: secrets
          secret:
            secretName: defing-secrets
            defaultMode: 0444
        - name: entrypoint
          configMap:
            name: defing-entrypoint
            defaultMode: 0444
  volumeClaimTemplates:
    - metadata:
        name: data
      spec:
        accessModes: ["ReadWriteOnce"]
        storageClassName: local-path        # K3s 默认 SC（省略亦可，默认即 local-path）
        resources:
          requests: { storage: 5Gi }        # redb + raft 日志 + 审计（100k 条）+ ring
```

### 9.6 `public-service.yaml`（数据面 + 管理面对外；K3s 三种暴露方式取一）

```yaml
apiVersion: v1
kind: Service
metadata:
  name: defing-public
  namespace: defing
  labels: { app: defing }
spec:
  selector: { app: defing }
  # 方式 A（推荐）：ClusterIP + Traefik Ingress（9.7）；gRPC 也走 Ingress h2c 或方式 B
  # 方式 B：type: LoadBalancer  → K3s ServiceLB 分配节点端口/外部 IP
  # 方式 C：type: NodePort      → 固定节点端口（裸机/无 LB 时）
  type: ClusterIP
  ports:
    - { name: http, port: 8384, targetPort: http }   # Admin UI + 数据面 HTTP/SSE
    - { name: grpc, port: 8383, targetPort: grpc }   # SDK gRPC 数据面
```

### 9.7 `ingress.yaml`（可选，K3s 内置 Traefik）

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: defing
  namespace: defing
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: web,websecure
    # gRPC 需要 HTTP/2：Traefik 对 h2c 后端需以下注解（或 gRPC 走 ServiceLB 直连）
    traefik.ingress.kubernetes.io/router.serversTransport: h2c@internal
spec:
  rules:
    - host: defing.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: defing-public
                port: { name: http }
  tls:
    - hosts: [defing.example.com]
      secretName: defing-tls   # cert-manager 或自签；K3s 默认无 cert-manager，需自装
```

> 简化建议：v1 直接用 `defing-public` 的 LoadBalancer（ServiceLB）同时暴露 8384/8383，Ingress 仅当需要域名 + TLS 时再加；gRPC 走 Traefik 需确认 HTTP/2 passthrough 配置，否则 gRPC 客户端直连 LB 端口更稳。

### 9.8 `pdb.yaml`（quorum 语义）

```yaml
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: defing-quorum
  namespace: defing
spec:
  minAvailable: 2          # 3 节点 quorum = ⌈3/2⌉ = 2；升级/驱逐最多同时下线 1 节点
  selector:
    matchLabels: { app: defing }
```

### 9.9 可选：监控（K3s 默认无 Prometheus）

- 安装 kube-prometheus-stack 后，`prometheus.io/scrape: "true"` 注解（已在 STS 中）即可被抓取 `/metrics`；
- 建议告警：`up == 0`（任一节点）、`current_leader 为 null 持续 > 15s`（节点日志已有周期提示，F10 相关）。

---

## 10. Phase 3 —— 部署步骤与验证清单

### 10.1 部署

```bash
kubectl apply -f namespace.yaml secret.yaml entrypoint-configmap.yaml \
  headless-service.yaml statefulset.yaml public-service.yaml pdb.yaml   # (+ ingress.yaml)

kubectl -n defing rollout status sts/defing --timeout=180s
kubectl -n defing get pods -o wide            # 期望 3/3 Running，分布在不同节点（反亲和 preferred）
kubectl -n defing logs defing-0 | head -30    # 期望 "cluster initialized from seed map (3 peers, all voters)"
kubectl -n defing logs defing-1 | head -30    # 期望 "cluster bootstrap delegated to a peer (catching up via replication)"
```

### 10.2 验证清单（命令级）

```bash
NS=defing
# 0) 就绪：readiness 全绿
kubectl -n $NS get sts defing -o jsonpath='{.status.readyReplicas}'

# 1) 集群成员（任意节点一致；3 voter + 1 leader）
# 镜像无 curl/wget（F9）→ 用端口转发 + 本机 curl
kubectl -n $NS port-forward svc/defing-public 8384:8384 & PF=$!
TOK=$(curl -s -X POST http://127.0.0.1:8384/api/v1/login -H 'Content-Type: application/json' \
  -d '{"password":"<ADMIN_PASSWORD>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
curl -s -H "Authorization: Bearer $TOK" http://127.0.0.1:8384/api/v1/cluster/members | python3 -m json.tool
# 期望：members 3 条（voter），current_leader 非空

# 2) 写路径（经 LB 命中任意节点；Phase 0 后 follower 写自动转发 → 200）
curl -s -X POST http://127.0.0.1:8384/api/v1/projects -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"name":"order-service"}'
# 重复若干次验证命中 follower 时仍 200（可观察 X-Defing-Forwarded-To 响应头）

# 3) 数据面（SDK 视角）：建项目令牌 → 拉快照
curl -s -X POST http://127.0.0.1:8384/api/v1/projects/order-service/tokens \
  -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{}'
PT=$(... 从响应取明文令牌 ...)
curl -s "http://127.0.0.1:8384/v1/projects/order-service/branches/dev/config?format=yaml" \
  -H "Authorization: Bearer $PT"

# 4) 容错：杀 leader Pod，观察重选举与继续写
LEADER=$(curl -s -H "Authorization: Bearer $TOK" http://127.0.0.1:8384/api/v1/cluster/members \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["current_leader"])')
kubectl -n $NS delete pod defing-$((LEADER-1)) --wait=false
sleep 15 && kubectl -n $NS get pods
# 新 leader 产生（成员 API current_leader 变化），写请求仍 200（中间件跟随新 leader_hint）

# 5) 重启恢复：全部 Pod 删除重建（同 data-dir 自动 resume，F2/F13）
kubectl -n $NS delete pod -l app=defing
kubectl -n $NS rollout status sts/defing --timeout=180s
# 项目/配置仍在（PVC + raft 复制），无 seed 冲突（seed 与成员表一致时不 WARN）
kill $PF 2>/dev/null
```

### 10.3 三语言 SDK 冒烟（集群内）

```bash
# 集群内任意 Pod 以 gRPC 端点池访问：
#   ConfigClient([{ grpc: 'defing-public.defing.svc:8383', http: 'http://defing-public.defing.svc:8384' }], { token })
# 读快照 + watch 订阅（事件经 raft 转发，任意节点可订阅，F7）
```

---

## 11. Phase 4 —— 运维规程

### 11.1 升级（镜像版本）

```bash
kubectl -n defing set image sts/defing defing=<registry>/defing:v0.2.0
kubectl -n defing rollout status sts/defing --timeout=300s
```

- PDB `minAvailable: 2` 保证任意时刻至多 1 个 Pod 下线（不丢 quorum）；leader 迁移期间写由中间件跟随新 leader_hint。
- 保守模式：`updateStrategy.type: OnDelete`，手动 `kubectl delete pod defing-<n>` 逐个滚动。

### 11.2 扩容（3 → 5）

```bash
kubectl -n defing scale sts defing --replicas=5
# defing-3 / defing-4（node 4/5）因 ordinal ≥ SEED_NODES=3 走 --join http://defing-0:8384（F11）
kubectl -n defing rollout status sts/defing
# 提升为 voter（join 默认 learner；管理员 Bearer）：
curl -X POST .../api/v1/cluster/promote -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"node_id":4}'
curl -X POST .../api/v1/cluster/promote -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"node_id":5}'
# 更新 PDB minAvailable 为 3（⌈5/2⌉）
```

> 注意：seed 不驱动运行期成员变更（`membership_diff` 仅 WARN）；扩容必须走 join/promote（代码事实 F2）。

### 11.3 缩容（5 → 3）

```bash
# 先移出成员表，再缩容（防止遗留 voter 配置）：
curl -X POST .../api/v1/cluster/remove -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"node_id":5}'
curl -X POST .../api/v1/cluster/remove -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"node_id":4}'
kubectl -n defing scale sts defing --replicas=3
# PDB 改回 minAvailable 2
```

### 11.4 节点/盘故障恢复（local-path 特性）

- local-path PVC 绑定创建它的节点；节点永久故障后 Pod 重建会 Pending（PVC 无法迁移）。
- **恢复流程**：`kubectl -n defing delete pvc data-defing-<n>`（确认该节点数据可从集群重建）→ Pod 重新调度 → 空卷启动 → 经 raft 复制自动追平（F2 resume/追平语义）。**数据不丢**：任意 2 个存活节点持有全量日志。
- 备份兜底：`defing admin snapshot --out /backup/snap-<ts>.json`（admin CLI/API），或定期 `kubectl cp`/velero 备份 PVC。

### 11.5 监控与排障

- `kubectl -n defing logs defing-<n> | grep -i leader`：选举/长时间无 leader 提示（B1，15s 无 leader 起每 10s 提示）。
- `/metrics`：Prometheus 抓取；`/readyz` 503 = raft 未初始化/追平中。
- 常见问题速查：
  - Pod 反复重启且日志 `join timed out`：leader 未就绪（检查其余 Pod/网络）；或 `409` = 成员表已有该 id（幂等成功，等待追平）。
  - 首次建群互相等待：确认三节点 seed 字符串完全一致（F2 校验，不一致首启即报错拒绝）。
  - 外部写 428：Phase 0 未合入时的预期行为；合入后仍出现则检查 leader 可达性（`X-Defing-Forwarded-To` 响应头）。

---

## 12. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| seed 不一致导致建群失败/脑裂 | 首启校验直接拒绝（F3）；resume 时 WARN 不覆盖 | seed 由脚本按 ordinal 确定性生成（§9.3），杜绝手抄不一致；变更拓扑走 join/remove 而非改 seed |
| local-path 单节点存储 | 节点故障 → 该节点 PVC 不可用 | raft 全量复制到 3 节点，删 PVC 重建自动追平（§11.4）；PDB/反亲和降低同节点共毁概率 |
| 升级/驱逐丢 quorum | 集群不可写 | PDB minAvailable=2 + StatefulSet 滚动（maxUnavailable=1）；OnDelete 保守模式可选 |
| 主密钥 ring 落只读卷 | 轮换失败/启动异常 | key 副本落 PVC（§6.2），禁止 Secret 只读卷直挂 `--master-key-file` |
| 管理面写 428（外部） | 写操作偶发失败 | Phase 0 写转发中间件（§7）；豁免清单回归测试 |
| gRPC 地址非法 | 进程启动失败 | `--grpc-addr 0.0.0.0:8383`（IP 字面量，F4） |
| 镜像拉取/私有 registry 认证 | Pod ImagePullBackOff | containerd `registries.yaml` 或 imagePullSecrets（§8） |
| `read_mode=linear` 外部读 428 | 读命中 follower 报错 | 方案默认 `stale`（不涉及）；linear 场景需另行设计外部读跟随（§13 后续工作） |
| Admin UI 写经 LB 未 pin 会话 | 无（会话令牌经 Raft 复制全集群有效；登录自动转发） | 无需处理 |

---

## 13. 决策记录（ADR 信号）

| # | 决策 | 理由（代码依据） | 替代被否原因 |
|---|---|---|---|
| D1 | 监听/成员表地址用 **pod 短主机名**（非 FQDN、非 0.0.0.0） | F3 字符串一致校验 + F4 可绑主机名；`/etc/hosts` 免 DNS 启动竞争 | FQDN 自绑定依赖 DNS 时序；0.0.0.0 被 seed 校验拒绝 |
| D2 | 建群用 `--bootstrap-peers`（非 bootstrap+join） | README 推荐；全员 voter 免 promote；幂等 resume 适配静态命令（F13） | join 流程需 promote，且首启顺序耦合 |
| D3 | 外部管理面写经**服务端转发中间件**解决 428 | F6：login/rotate 已有同构转发先例；客户端（Admin UI/curl）无跟随实现且 pod DNS 外部不可解析 | 客户端跟随（不可行）；leader 感知网关（过重） |
| D4 | 主密钥文件落 PVC（entrypoint 复制），非只读 Secret 挂载 | F8：ring 必须可写持久；在线轮换依赖 `--master-key-file` | 只读挂载导致 ring 写失败；纯 env 禁用轮换 |
| D5 | gRPC 绑 `0.0.0.0:8383` | F4/F5：gRPC 必须 IP 字面量且不落成员表 | 绑短主机名不可行（parse 拒绝） |
| D6 | PDB `minAvailable: 2` + RollingUpdate + 反亲和 | quorum 语义（⌈3/2⌉）；research §3.3-3.4 | 无 PDB 时驱逐可同时下线多数派 |
| D7 | 扩容 = ordinal≥SEED_NODES 的 Pod `--join`；缩容先 remove 再 scale | F2/F11：seed 不驱动运行期成员变更 | 直接 scale 会遗留配置/learner 悬挂 |

---

## 14. 后续工作（roadmap 对照 research-k8s-k3s-integration.md §7）

1. **官方 Helm chart**（research §7.2）：把 §9 manifests 参数化为 chart（values：replicas/registry/域名/存储大小），并在 K3s 上经 HelmChart CRD 一键分发（§4 K3s 差异）。
2. **写转发中间件合入后**：将 §10.2 的"LB 写验证"纳入 `scripts/api-surface-test.sh` 或 CI（.github/workflows）。
3. **Sync Controller / Sidecar**（research §7.1/§7.4）：配置下发 K8s 化，与本文集群部署正交。
4. **linear 读模式的外部访问**：若启用 `--read-mode linear`，需为 GET 428 提供类似转发放行（或文档化客户端跟随），当前方案按默认 `stale` 交付。

---

## 15. Execution Readiness View（执行交接视图）

```text
Execution Readiness View:
- Intent Lock: K3s 上以 StatefulSet 部署 Defing 3 节点集群，数据面/管理面对外可用，安全/持久化/PDB/运维齐备
- Scope Fence: 仅方案文档；含 Phase 0 中间件任务（未执行）；不含 Helm chart、K3s 控制面高可用、linear 读、Sync Controller
- Baseline Lock: §0 Baseline/Authority Refs（research-k8s-k3s-integration、cluster-bootstrap、defing-cluster、README、dsh-cli/dsh-api/dsh-crypto 源码）
- Approved Behavior: 见 §1 结论摘要与 §7 中间件契约（豁免清单、单次转发、失败回落）
- Owner / Contract Constraints: seed 校验规则（F3）、gRPC 字面量（F4）、428 契约（F6/F11）、ring 可写（F8）不可违反
- Compatibility Boundary: 既有 CLI 语义不变；login/rotate/join 行为不变
- Retirement Boundary: 中间件为新增行为，合入后可用 `X-Defing-Forwarded-To` 观测；回滚 = 移除挂载行
- Task Batches: Phase 0（中间件+测试）→ Phase 1（镜像）→ Phase 2（manifests）→ Phase 3（部署验证）→ Phase 4（运维规程）
- Test Obligations: `cargo test -p dsh-api --test leader_write_forward` + `cluster-demo.sh`/`api-surface-test.sh` 回归；§10.2 部署验证清单
- Review Gates: Phase 0 合入前 code review（dsh-api 中间件）；生产 apply 前核对 Secret 值/域名/镜像
- Drift / Rewind Rules: seed 只用于首启，运行期拓扑变更一律走 join/promote/remove API；发现 seed 漂移仅 WARN，勿手工改成员表
- Evidence Required Before Completion: §10.2 全项通过（就绪、成员、LB 写 200、容错、重启恢复）
- Advisory Boundary: 本视图为执行交接提示，非完成权/门禁
```
