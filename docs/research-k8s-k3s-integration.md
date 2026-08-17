# Defing 集成 Kubernetes / K3s 生态调研报告

> 研究对象：Defing —— Rust 编写的开源自建分布式配置服务（Raft 集群，HTTP + SSE watch + gRPC 数据面；配置按 项目→分支→分组→item 组织，支持 YAML/TOML/JSON 渲染；发布走 草稿→版本→发布→通知 闭环）。
>
> 调研范围：外部配置服务如何"方便地"接入 K8s/K3s 的配置下发、变更触发、Helm 打包、K3s 差异、GitOps 互动。
> 本文所有 URL 均来自 web_search 真实结果，且经 HTTP 可达性验证（2025 年检索）。

---

## 0. 结论摘要（TL;DR）

- **首选路线**：为 Defing 开发一个**声明式同步控制器（Operator）**，把 `(project, branch)` 的渲染产物同步为 ConfigMap/Secret，这是 Nacos/Apollo 生态在 K8s 上落地的主流方式（nacos-controller 双向同步、Apollo 官方 K8s ConfigMap 指南）。重点要解决**防回写死循环**（label/annotation 标记 + hash 比对 + ownerRef 纪律）。
- **次选**：**Sidecar 模式**与 Defing 现有的 SSE/gRPC watch 能力天然契合，可参考 Vault Agent Injector / consul-template injector 的注解注入模型。
- **变更触发**：同步控制器自身在写入 CM 后调用 `kubectl rollout restart` 式滚动重启，或用 stakater Reloader 这类通用工具解耦（注意：`kubectl rollout restart` 并非 1.27 新增，而是长期存在的原生命令）。
- **Helm 打包**：参考 etcd 官方 K8s 教程与 vault-helm / consul-k8s：StatefulSet + Headless Service + 脚本化 `--join` 自动发现 + PDB（quorum 语义）+ 反亲和 + 优雅终止。
- **K3s**：利用内置 helm-controller（HelmChart CRD）免 Helm CLI 分发；默认 local-path 存储可直接用；单节点/边缘场景可退化为 systemd 单进程。
- **GitOps**：同步控制器/Operator 本身完全可以被 ArgoCD/Flux 纳管（ArgoCD cluster bootstrapping、Flux HelmRelease），但要处理"GitOps 循环 vs 控制器循环"的双写冲突。

---

## 1. 配置下发模式（外部配置服务 → K8s）

### 1.1 同步控制器 / Operator 模式（推荐）

**工作原理**

1. 控制器以 Deployment 形式运行在集群内，用 RBAC 获得对 ConfigMap/Secret 的读写权限。
2. 监听 Defing 侧变化：HTTP/SSE watch 或 gRPC watch `(project, branch)`，拿到渲染后的 YAML/TOML/JSON 文本；也可定期轮询兜底。
3. 将渲染结果写入目标 namespace 的 ConfigMap（非敏感配置）或 Secret（敏感 item，如 token/私钥）。
4. 可选反向通道：watch ConfigMap 被应用修改后的变化，推回 Defing（发布新版本），形成双向同步（nacos-controller 即此模型）。

**防回写死循环（关键工程点）**

- **标记所有权**：控制器只 reconcile 带 `defing.io/managed: "true"` label（或 annotation）的 ConfigMap，绝不触碰未标记的资源——这是"控制器不覆盖应用/用户手工修改"的第一道闸。
- **内容 hash 比对**：reconcile 时计算"期望内容 hash"与 ConfigMap 当前内容的比对，无变化不写（避免 写→watch 事件→再 reconcile→再写 的自激振荡）；可把 hash 存入 annotation 作为幂等依据。
- **ownerRef 纪律**：不要把 ConfigMap 的 ownerRef 指向会被 GitOps/应用随意替换的对象；如果确需级联清理，ownerRef 应指向控制器自己维护的、生命周期稳定的对象（如每个 `(project,branch)` 对应的自定义资源 DefingProject），或者干脆不用 ownerRef、靠 label selector 清理。
- **方向选择**：单向下发（Defing→K8s）时，对"应用侧手工改了 CM"采取忽略策略（保留修改，仅记录事件）；双向同步时需显式配置冲突策略（Defing 覆盖 or 应用覆盖），nacos-controller 提供可配置的覆盖行为。
- **只读性提示**：若 CM 内容完全由控制器生成，可考虑 `immutable: true`（K8s 1.21+ GA），彻底杜绝运行期改动与回写问题，代价是变更必须重建 CM（controller 删除重建即可）。

**优点**：声明式、与 K8s 原生对象对齐，应用零改造（照常挂 ConfigMap）；可观测性好（事件/status）；可被 GitOps 纳管；能同时覆盖 Secret。
**缺点**：需要开发和维护一个控制器（Go/Rust 均可，K8s client-go / kube-rs）；CM 内容变更不自动触发 Pod 重启，需配合 §2 的触发机制；Secret 明文落盘于 etcd（K8s 原生限制，敏感场景建议配合加密）。
**工作量**：M（控制器骨架 + watch + 渲染 + 幂等写 + 测试；若复用 controller-runtime 模板则接近 L 中的 M+）。

**真实项目参考**

| 项目 | 模式 | URL |
|---|---|---|
| nacos-controller（Nacos 官方生态） | Nacos ↔ K8s ConfigMap **双向**同步控制器，支持多 namespace、覆盖策略 | https://github.com/nacos-group/nacos-controller |
| Apollo K8s ConfigMap 用户指南（官方文档） | Apollo Java 客户端直接消费 K8s ConfigMap，外部配置中心与 K8s 的桥接模式 | https://github.com/apolloconfig/apollo/blob/master/docs/en/client/k8s-configmap-user-guide.md |
| consul-k8s（Service Sync） | ⚠️ 注意：Consul 官方 k8s 同步的是**服务注册**（syncCatalog），**不是配置**；Consul 配置下发靠 consul-template 侧车或应用直连 Consul API | https://developer.hashicorp.com/consul/docs/k8s/service-sync |

> 参考竞品结论：Nacos 生态把"ConfigMap 同步"做成了官方控制器；Apollo 官方文档支持客户端直读 K8s ConfigMap；Consul 官方并未提供配置→ConfigMap 的同步器（其 K8s 集成聚焦服务网格/服务同步）。**这恰是 Defing 的差异化机会点**：做一个配置服务原生的 K8s 同步控制器，比 Nacos 的双向模型更简单（只做下发 + 可选回写）。

---

### 1.2 Sidecar 模式

**工作原理**

- 给业务 Pod 注入一个 sidecar（或与主容器同 Pod），sidecar 通过 Defing 的 **gRPC/SSE watch** 订阅 `(project, branch)`，把渲染结果写入共享的 `emptyDir` 卷中的本地文件（如 `/etc/defing/app.yaml`）。
- 变更触发两种策略：a) 写文件 + 发 SIGHUP 让主进程重载；b) 写文件后通过 K8s API 触发本 Pod 重启（类似 reloader 的单 Pod 版）。
- 注入方式可做两层：手工加容器（工作量小），或做一个 **MutatingAdmissionWebhook** 按 annotation（如 `defing.inject/true`）自动注入（对齐 Vault Agent Injector 体验，工作量中）。

**优点**：应用零改造即可获得 watch/热更新能力（Defing 的 SSE/gRPC watch 是最契合这一模式的能力）；不经过 etcd，配置不落 ConfigMap；支持把 Secret 直接写内存/受限卷。
**缺点**：每 Pod 一个 sidecar 有资源开销；配置不集中（集群里没有 ConfigMap 视图）；故障排查链路变长；Webhook 注入方案还要维护 webhook 证书/RBAC。
**工作量**：手工 sidecar S～M；annotation 注入 Webhook M。

**真实项目参考**

- Vault Agent Sidecar Injector（HashiCorp 官方）：MutatingWebhook + annotation（`vault.hashicorp.com/agent-inject: "true"`）注入 Vault Agent，渲染 Secret 到共享卷：https://developer.hashicorp.com/vault/docs/platform/k8s/injector
- Trendyol consul-template-injector：把 consul-template 作为 init + sidecar 注入的 webhook 实现，可直接借鉴架构：https://github.com/Trendyol/trendyol-consul-template-injector
- Spring Cloud Kubernetes：应用内直接 watch K8s ConfigMap/Secret 并热刷新（应用侧 watch，与"外部配置中心"是互补的消费端模式）：https://docs.spring.io/spring-cloud-kubernetes/reference/

---

### 1.3 Init 容器一次性拉取

**工作原理**：Pod 启动时先跑一个 init 容器，调用 Defing gRPC/HTTP 拉取 `(project, branch)` 渲染产物，写入 `emptyDir` 共享卷；主容器挂载该卷直接读取。配置只在启动时定格，不 watch。

**优点**：实现极简（一个脚本/一个小二进制即可）；无常驻 sidecar 开销；适合"启动即定"的配置。
**缺点**：无热更新，变更需重新调度 Pod；若 Defing 不可用则 Pod 启动失败（可配 `restartPolicy` 与超时策略缓解）。
**工作量**：S。

**参考**：该模式是 K8s 社区通用模式（官方 StatefulSet 教程中 etcd 也用 init 脚本做引导，见 §3）；无专属官方组件，直接手写即可。

---

### 1.4 CSI 驱动挂载

**工作原理**：实现一个 CSI（Container Storage Interface）volume driver（gRPC 协议），或更务实——为 [Secrets Store CSI Driver](https://secrets-store-csi-driver.sigs.k8s.io/introduction) 实现一个 **provider**：应用声明 `SecretProviderClass`（描述 Defing 的 project/branch/item 来源），kubelet 在挂载时调用 provider 从 Defing 拉取渲染内容并挂载为文件；可配置轮转（rotation）与同步到 K8s Secret。

**何时才值得做**：a) 已有 CSI 基础设施（如云厂商 Secret Store CSI 已部署），只想加一个数据源 provider；b) 对"配置不落 etcd"有硬性合规要求；c) 需要细粒度轮转/吊销。**何时不值得**：只有一两个集群、没有专门 SRE 团队时，完整实现 CSI 驱动（含 NodePublishVolume、健康上报、升级兼容）成本很高，远不如 §1.1 控制器。

**优点**：挂载即文件，应用零改造；配置不进 etcd（CSI 直挂）；生态成熟（云厂商均基于此框架）。
**缺点**：实现/运维成本高；调试链路复杂；旋转与多副本一致性需额外设计。
**工作量**：完整 CSI 驱动 L；基于 secrets-store-csi-driver 写 provider M。

**真实项目参考**：Secrets Store CSI Driver（kubernetes-sigs 官方）：https://secrets-store-csi-driver.sigs.k8s.io/introduction

---

## 2. 变更触发（配置更新 → Pod 滚动重启）

背景事实：**ConfigMap/Secret 内容更新不会自动重启 Pod**（volume 会更新，进程是否重载由应用决定）。因此需要显式触发。

| 方案 | 原理 | 适用 | 工作量 |
|---|---|---|---|
| **stakater Reloader** | watch 集群内 ConfigMap/Secret 的变更，按 annotation（`reloader.stakater.com/auto: "true"` 或 `reloader.stakater.com/search`）对引用了它们的 Deployment/StatefulSet 触发滚动重启；自带 RBAC 与忽略机制 | 通用、与 §1.1 控制器天然配套（控制器写 CM → Reloader 重启） | S |
| **Keel** | 主要做**镜像**自动化更新（tag 策略、语义化版本、Helm release 升级），也支持配置/依赖变化触发，偏"持续交付"语义 | 已有 Keel 的团队可顺带复用；单独为配置引入则过重 | S |
| **`kubectl rollout restart`（原生）** | 手动/脚本调用，给 Pod template 注入新的 `pod-template-hash` 触发滚动；**注意：这是自 K8s 1.15 就有的原生命令，并非 1.27 新增**（1.27 相关的新特性是 sidecar 容器、in-place resize 等，与配置触发无关） | 同步控制器写完 CM 后直接调 `kubectl rollout restart deployment/xxx`（或调用 K8s API 打 `kubectl.kubernetes.io/restartedAt` annotation）是最简单可控的"原生手法" | S |
| 控制器内置触发 | 同步控制器在写入 CM/Secret 后，直接对目标工作负载执行滚动（打 annotation 触发），无需第三方 | 想要"一个组件全包"时 | S（在 §1.1 控制器内实现） |

**真实项目参考**
- Reloader 官方文档（工作原理、annotation 参考、FAQ）：https://docs.stakater.com/reloader/latest/architecture/how-it-works.html
- Keel 官网：https://keel.sh/
- kubectl rollout restart 官方参考：https://kubernetes.io/docs/reference/kubectl/generated/kubectl_rollout/kubectl_rollout_restart/

> 建议：Defing 同步控制器 v1 自带"写 CM 后对关联工作负载打 `restartedAt` annotation 触发滚动"（S 工作量），并保留"不自动重启、交给 Reloader"的开关，兼顾开箱即用与解耦。

---

## 3. Helm 打包 Stateful 应用（Raft 集群）最佳实践

Defing 是 Raft 集群，参考 etcd/consul/vault 在 K8s 上的官方做法，核心清单如下。

### 3.1 StatefulSet + Headless Service（稳定网络标识）

- StatefulSet 的 `spec.serviceName` 指向 Headless Service（`clusterIP: None`），Pod 获得稳定 DNS：`defing-<ordinal>.defing.<namespace>.svc.cluster.local`。
- **Raft 节点自动发现（--join 无需硬编码 IP）**：官方教程（etcd）的做法是启动脚本解析 `$(hostname)` 的序号（如 `defing-2` → ordinal 2），再循环 0..replicas 生成全部 peer 地址列表，作为 `--initial-cluster` / `--join` 参数。Defing 可同样在 entrypoint 脚本或 init 容器中：`for i in $(seq 0 $((REPLICAS-1))); do peers="$peers,defing-$i.defing.svc:$RAFT_PORT"; done`。首次启动用 `--initial-cluster`，后续节点用 `--join`。
- Headless Service 同时暴露 gRPC/HTTP 数据面（普通 ClusterIP Service 也可另建一个用于负载均衡）。

### 3.2 持久化（PVC）

- 用 `volumeClaimTemplates` 自动为每个副本创建 PVC（`data-defing-<ordinal>`）；`storageClassName` 参数化（K3s 默认 local-path 即可，见 §4）。
- 若 Raft 有独立的 WAL/snapshot 目录，可拆两个卷模板。

### 3.3 PodDisruptionBudget（quorum 语义）

- 3 节点集群必须保证至少 2 个成员存活才不丢 quorum：`minAvailable: 2`（公式 `⌈n/2⌉` 即 `(n+1)/2`）。配合 `maxUnavailable` 语义，避免节点维护/驱逐时同时下线多数派。

### 3.4 反亲和与拓扑

- `podAntiAffinity`：3 节点副本尽量/必须分散到不同节点（`topologyKey: kubernetes.io/hostname`）；多云/多可用区再加 `topology.kubernetes.io/zone`。
- 可选 `topologySpreadConstraints` 替代/增强反亲和。

### 3.5 优雅终止

- `terminationGracePeriodSeconds`：给 Raft 节点留出离开集群/落盘的时间（如 30–60s）；配 `preStop` hook 调用 Defing 的 leave/shutdown RPC（若无此 RPC，Raft 会自动超时剔除，但优雅退出可避免 leader 抖动）。
- 升级策略：共识类应用建议 `OnDelete`（一次手动删一个 Pod 滚动升级）或谨慎使用 RollingUpdate；vault-helm 对 Raft 节点的升级也强调按序、保证 quorum。

### 3.6 安全上下文

- `runAsNonRoot: true`、`readOnlyRootFilesystem: true`（数据目录单独挂载可写卷）、`allowPrivilegeEscalation: false`、`capabilities.drop: ["ALL"]`；`seccompProfile: RuntimeDefault`。Rust 单二进制天然适合无依赖镜像（distroless/scratch + 非 root 用户）。

### 3.7 参考实现

- Kubernetes 官方教程《运行一个有状态复制应用（etcd）》——StatefulSet + Headless Service + init 脚本生成 `--initial-cluster`/`--join` 的完整样板：https://kubernetes.io/docs/tutorials/stateful-application/run-replicated-stateful-application/
- StatefulSet 官方概念文档：https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/
- PodDisruptionBudget 官方任务文档：https://kubernetes.io/docs/tasks/run-application/configure-pdb/
- vault-helm（HA + Raft integrated storage，含 StatefulSet、PDB、反亲和、注入器）：https://developer.hashicorp.com/vault/docs/platform/k8s/helm
- Consul on Kubernetes 控制面架构文档（server 集群在 K8s 上的部署模型）：https://developer.hashicorp.com/consul/docs/architecture/control-plane/k8s

**工作量**：M（一个 chart + 启动脚本 + 若干模板；若只想快速上线可先 S，后续补 PDB/反亲和）。

---

## 4. K3s 特有差异

| 维度 | K3s 情况 | 对 Defing 的影响/做法 |
|---|---|---|
| **内置 helm-controller** | K3s 内置 helm-controller，提供 `HelmChart`/`HelmChartConfig` CRD：把 HelmChart 清单（含 chart 来源、values）放入 `/var/lib/rancher/k3s/server/manifests/` 或直接 `kubectl apply`，K3s 自动用内置 Helm 安装/升级 chart，**无需 Helm CLI** | Defing chart 直接以 HelmChart CRD 分发：`kubectl apply -f defing-helmchart.yaml` 即可在 K3s 上装集群，也可用 `HelmChartConfig` 覆盖 values |
| **轻量单二进制** | server/agent 合一单二进制，默认带 SQLite 存储、内置 Traefik ingress、内置 local-path 存储类、内置 ServiceLB | 部署 Defing 集群没有任何特殊适配；Traefik 可直接做 gRPC/HTTP ingress |
| **local-path 存储** | 默认 StorageClass `local-path`（Rancher local-path-provisioner），PVC 直接落节点磁盘 | Defing 的 volumeClaimTemplates 用默认 SC 即可，无需单独准备存储 |
| **嵌入式 SQLite vs etcd** | K3s 控制面数据存储通过 kine 抽象：单 server 默认 SQLite（写入被 kine 转成 etcd API）；`--cluster-init` 可启用嵌入式 etcd；也可接外部数据库。这是 **K3s 控制面**的存储，与 Defing 自身 Raft 数据无关 | 无需为 Defing 做适配；仅需知道：多 server 高可用要用 `--cluster-init`（embedded etcd）或外部 DB |
| **低资源/边缘/ARM** | K3s 支持 amd64/arm64/armhf，官方定位轻量边缘发行版；Rust 静态单二进制 + 无 JVM 依赖，非常适合 | 单节点场景：a) K3s + Deployment（1 副本 Raft 单成员），享受 K8s API/运维一致性；b) 更极致的边缘/资源受限场景：直接 systemd 服务跑 Defing 二进制（跳过 K8s 层），客户端仍走 gRPC 数据面。权衡：前者统一运维、后者省掉 K8s 开销 |

**真实项目参考**
- K3s Helm 文档（helm-controller / HelmChart / HelmChartConfig）：https://docs.k3s.io/add-ons/helm
- K3s 集群数据存储文档（SQLite / embedded etcd / 外部 DB）：https://docs.k3s.io/datastore
- K3s 网络文档（默认 Traefik ingress、ServiceLB）：https://docs.k3s.io/networking
- local-path-provisioner：https://github.com/rancher/local-path-provisioner

**工作量**：S（K3s 侧零改动；HelmChart CRD 分发样例 + 文档即可）。

---

## 5. GitOps 互动（ArgoCD / Flux 纳管同步控制器本身）

**结论：可以，且是推荐做法。** 同步控制器/Operator 本质是"工作负载 + RBAC + CRD"，完全可以用 GitOps 声明式纳管；但必须处理"双控制循环"冲突。

**ArgoCD**

- 官方文档明确支持"用 ArgoCD 管理 ArgoCD 及集群内基础设施"（cluster bootstrapping / App of Apps）：把"同步控制器所在的命名空间"作为一个 Application（Helm chart 或 Kustomize），Controller 的 Deployment、RBAC、CRD 全部声明在 Git 中。
- 关键配置：`ignoreDifferences`（忽略 controller 自己写入的 status 等字段）、安装命名空间与业务命名空间分离、`syncPolicy` 控制自动同步。

**Flux**

- 用 `Kustomization` 纳管 manifest、`HelmRelease` 纳管 Helm chart（自动升级/回滚/卸载）：https://fluxcd.io/flux/components/helm/helmreleases/ 。

**双控制循环冲突与解法**（这是本节的实质难点）

- 冲突形态：GitOps 工具把某个 ConfigMap 内容当"期望状态"反复恢复，而同步控制器也在写同一个 ConfigMap → 两个控制器打架。
- 解法三选一/组合：
  1. **范围分离**：GitOps 只管"controller 自身"（Deployment/RBAC/CRD），ConfigMap 的增删改**完全交给同步控制器**（CM 上打 `defing.io/managed` label，Git 中不放这些 CM 的期望内容）。
  2. **方向分离**：若要求"配置的最终事实源是 Git"，则退化为"Git → Defing"（CI 把 Git 配置发布进 Defing），K8s 侧 CM 仍由 Defing 同步控制器生成——Git 与 CM 之间经 Defing 中转，不直接冲突。
  3. **ignoreDifferences/忽略注解**：Git 声明 CM 骨架，但用 ArgoCD ignoreDifferences 或 Flux 的 managedFields 策略，放行同步控制器对内容的修改。

**真实项目参考**
- ArgoCD Cluster Bootstrapping 官方文档：https://argo-cd.readthedocs.io/en/stable/operator-manual/cluster-bootstrapping/
- Flux HelmRelease 官方文档：https://fluxcd.io/flux/components/helm/helmreleases/
- CNCF 官方博客《GitOps and mutating policies: the tale of two loops》（双循环/变更策略的权威讨论）：https://www.cncf.io/blog/2024/01/18/gitops-and-mutating-policies-the-tale-of-two-loops/

**工作量**：S–M（主要为文档与样例编排；控制器本身无需为此改造，只需遵守 label/ownerRef 纪律）。

---

## 6. 集成方案横向对比

| 方案 | 原理 | 应用改造 | 热更新 | 优点 | 缺点 | 工作量 | 推荐度 |
|---|---|---|---|---|---|---|---|
| 同步控制器（Operator） | Defing → ConfigMap/Secret | 零 | 需配触发（§2） | 声明式、可观测、可 GitOps、兼管 Secret | 需自研控制器；明文落 etcd | M | ★★★★★（首选） |
| Sidecar（手工/Webhook） | sidecar watch Defing 写共享卷 | 零 | 天然（SSE/gRPC watch） | 与 watch 能力契合；配置不进 etcd | 每 Pod 开销；无集群视图；Webhook 复杂 | S/M | ★★★★ |
| Init 容器 | 启动时拉一次 | 零 | 无 | 极简 | 变更需重启 Pod；依赖可用性 | S | ★★★ |
| CSI 驱动 | CSI 挂载 Defing 渲染文件 | 零 | 可（rotation） | 不进 etcd；生态成熟 | 实现/运维成本高 | M（provider）/ L（driver） | ★★（有条件才做） |
| Reloader/Keel/rollout restart | CM 变更→滚动重启 | 零 | 触发式 | 解耦、通用 | 是"触发器"而非"下发器" | S | ★★★★★（与①配套） |

---

## 7. 对 Defing 的落地建议（Roadmap 草案）

1. **v1（对齐 K8s 核心价值）**：Defing Sync Controller（Rust 用 kube-rs，或 Go 用 controller-runtime）——watch `(project, branch)` 渲染产物 → ConfigMap/Secret；label 标记 + hash 幂等 + 忽略非托管 CM（防回写死循环）；写后按需触发滚动（restartedAt annotation，可关闭）。
2. **v1 配套**：官方 Helm chart（StatefulSet + Headless + volumeClaimTemplates + PDB minAvailable=(n+1)/2 + 反亲和 + terminationGracePeriod + preStop 优雅退出 + 非 root 安全上下文），启动脚本按 ordinal 自动生成 Raft peer 列表（参照 etcd 官方教程）。
3. **K3s 专项**：HelmChart CRD 一键安装示例 + HelmChartConfig 覆盖指南；默认 local-path 即插即用；文档说明单节点/边缘场景的"K3s+Deployment vs systemd 单进程"取舍。
4. **v2（增强）**：annotation 注入式 Sidecar（对齐 Vault Agent Injector），复用 gRPC/SSE watch；SSE 事件可同时驱动业务侧热加载。
5. **远期**：若出现硬性"配置不进 etcd"需求，再评估基于 Secrets Store CSI Driver 的 provider。
6. **GitOps 章**：提供"controller 自身"的 ArgoCD Application / Flux HelmRelease 样例，明确 CM 归属边界（§5 三方案）。

---

## 附录：参考 URL 总表（全部经 web_search 获得并验证可达）

1. K3s Helm（helm-controller / HelmChart）：https://docs.k3s.io/add-ons/helm
2. K3s 集群数据存储（SQLite / embedded etcd）：https://docs.k3s.io/datastore
3. K3s 网络（默认 Traefik）：https://docs.k3s.io/networking
4. nacos-controller（Nacos↔K8s ConfigMap 双向同步）：https://github.com/nacos-group/nacos-controller
5. Apollo 官方 K8s ConfigMap 用户指南：https://github.com/apolloconfig/apollo/blob/master/docs/en/client/k8s-configmap-user-guide.md
6. consul-k8s Service Sync（注意：同步服务而非配置）：https://developer.hashicorp.com/consul/docs/k8s/service-sync
7. Vault Agent Sidecar Injector 官方文档：https://developer.hashicorp.com/vault/docs/platform/k8s/injector
8. vault-helm（HA/Raft/PDB/StatefulSet）：https://developer.hashicorp.com/vault/docs/platform/k8s/helm
9. Consul on Kubernetes 控制面架构：https://developer.hashicorp.com/consul/docs/architecture/control-plane/k8s
10. stakater Reloader 官方文档：https://docs.stakater.com/reloader/latest/architecture/how-it-works.html
11. Spring Cloud Kubernetes 官方参考：https://docs.spring.io/spring-cloud-kubernetes/reference/
12. Secrets Store CSI Driver：https://secrets-store-csi-driver.sigs.k8s.io/introduction
13. Keel 官网：https://keel.sh/
14. kubectl rollout restart 官方参考：https://kubernetes.io/docs/reference/kubectl/generated/kubectl_rollout/kubectl_rollout_restart/
15. StatefulSet 官方概念文档：https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/
16. PodDisruptionBudget 官方任务文档：https://kubernetes.io/docs/tasks/run-application/configure-pdb/
17. K8s 官方教程：运行有状态复制应用（etcd，Raft 引导样板）：https://kubernetes.io/docs/tutorials/stateful-application/run-replicated-stateful-application/
18. local-path-provisioner：https://github.com/rancher/local-path-provisioner
19. ArgoCD Cluster Bootstrapping：https://argo-cd.readthedocs.io/en/stable/operator-manual/cluster-bootstrapping/
20. Flux HelmRelease：https://fluxcd.io/flux/components/helm/helmreleases/
21. CNCF 博客《GitOps and mutating policies: the tale of two loops》：https://www.cncf.io/blog/2024/01/18/gitops-and-mutating-policies-the-tale-of-two-loops/
22. Trendyol consul-template-injector（Sidecar 注入参考实现）：https://github.com/Trendyol/trendyol-consul-template-injector
