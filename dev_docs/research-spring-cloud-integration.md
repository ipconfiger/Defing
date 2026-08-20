# Defing 集成 Spring Cloud 生态调研报告

> 研究对象：Defing —— Rust 编写的开源自建分布式配置服务（Raft 集群，HTTP + SSE watch + gRPC 数据面；配置按 项目→分支→分组→item 组织，支持 YAML/TOML/JSON 渲染；发布走 草稿→版本→发布→通知 闭环）。
>
> 调研范围：外部配置服务如何"方便地"接入 Spring Cloud 生态（Spring Boot 2.x/3.x + Spring Cloud），重点覆盖 Config Server 的 EnvironmentRepository 抽象、spring-cloud-kubernetes、Nacos/Consul/ZooKeeper 客户端模型，以及三条集成路径（Java SDK + Starter / Config Server 代理 / K8s 同步控制器）的对比。
> 本文所有 URL 均来自 web_search 真实结果（Spring 官方文档优先）。

---

## 0. 结论摘要（TL;DR）

- **首选路线（路径 A）**：为 Defing 写 **Java SDK + Spring Boot Starter（客户端直连模式）**，照抄 Nacos/Consul 的"客户端 PropertySource + watch → `RefreshEvent` → `@RefreshScope`"范式，Defing 的 SSE/gRPC watch 推送能力 100% 保留，无 K8s/MQ 依赖，工作量 M。
- **次选（路径 B）**：实现一个 **Spring Cloud Config Server**（Defing 作为 `EnvironmentRepository` backend），客户端零 SDK（用官方 `spring-cloud-config-client`），但 Config Server 协议是**拉取式**的，推送需 webhook→`/monitor`→Spring Cloud Bus+MQ 整条链路补强，工作量 M。
- **有条件方案（路径 C）**：**K8s 同步控制器**把 Defing 渲染结果写入 ConfigMap/Secret，Spring 应用用 spring-cloud-kubernetes 零代码消费；强依赖 K8s、最终一致、秒级延迟，工作量 L。
- **模型映射**：项目→application/namespace、分支→profile/group、分组→label/dataId、item→property key（详见 §3、§6）。
- **协议兼容**：Config Server 协议（`/{application}/{profile}/{label}` 等端点）简单稳定，走路径 B 时值得兼容；它不承载推送语义，走路径 A 时可不兼容，但可低成本附带兼容端点换取生态兼容性（A+B 组合）。
- **refresh 最后一公里**：无论哪条路径，热更新最终都落在 `@RefreshScope` + `EnvironmentChangeEvent`（spring-cloud-context）上；多实例广播可复用 Spring Cloud Bus。

---

## 1. Spring Cloud Config Server 生态

### 1.1 EnvironmentRepository 抽象 —— 一切 backend 的接入点

Spring Cloud Config Server 的核心抽象是 `EnvironmentRepository` 接口，职责是把 `(application, profile, label)` 三元组解析为一个 `Environment`（内含按序排列的多个 `PropertySource`，每个 `PropertySource` 是一个 `name → Map<key,value>`）：

```java
public interface EnvironmentRepository {
    Environment findOne(String application, String profile, String label);
    // 新版本另有带 includeOrigin 等重载
}
```

- **Git 后端**（默认）：`JGitEnvironmentRepository`（`spring.profiles.active=git`），把 label 当分支/commit 检出并解析 property 文件。官方文档：[Git Backend](https://docs.spring.io/spring-cloud-config/reference/server/environment-repository/git-backend.html)。
- **Consul / ZooKeeper / Vault / JDBC / Redis / AWS 等后端**：通过 `spring.profiles.active=consul|zookeeper|vault|jdbc|...` 激活各自的 `EnvironmentRepository` 实现，多个后端可同时生效，由 **`CompositeEnvironmentRepository`** 按 `spring.cloud.config.server.composite` 或 profile 顺序合并。官方文档：[Composite Environment Repositories](https://docs.spring.io/spring-cloud-config/reference/4.2/server/environment-repository/composite-repositories.html)。
- **Nacos 不是 Config Server 的 server 端 backend**：Nacos 只以"客户端 PropertySource"方式接入（见 §3），spring-cloud-config 官方仓库里没有 Nacos 的 EnvironmentRepository 实现——这点对路径 B 的选型很重要。

**自定义服务成为 Config Server backend 的标准做法**（官方文档 [Custom Environment Repositories](https://docs.spring.io/spring-cloud-config/reference/4.1/server/environment-repository/custom-enviroment-repository.html)、源码文档 [custom-enviroment-repository.adoc](https://github.com/spring-cloud/spring-cloud-config/blob/v4.2.4/dev_docs/modules/ROOT/pages/server/environment-repository/custom-enviroment-repository.adoc)）：

1. 实现 `EnvironmentRepository`（如需 label/附加信息还可实现 `EnvironmentRepositorySearchPathLocator`）；
2. 用 `@Bean` 注册并 `@Import` 进 Config Server 主应用；
3. 关掉默认 git backend（如把 `spring.cloud.config.server.git.uri` 留空，或用 `spring.profiles.active=native`、或走 composite 并显式排序）；
4. 客户端无感知——它们只访问 Config Server 的 HTTP 端点。

### 1.2 客户端 refresh 机制（三层）

1. **`@RefreshScope` + `/actuator/refresh`**：`spring-cloud-context` 提供 `RefreshScope`、`RefreshEvent`、`EnvironmentChangeEvent`。POST `/actuator/refresh`（`spring-cloud-starter-actuator`）触发 `RefreshScope.refreshAll()` → 重建 `@RefreshScope` 的 bean → `@ConfigurationProperties`（配合 `@RefreshScope`）重新绑定。这是所有方案共用的"最后一公里"。
2. **Spring Cloud Bus**：`spring-cloud-starter-bus-amqp|kafka` 让 `/actuator/busrefresh` 把 `RefreshRemoteApplicationEvent` 广播给集群内所有实例（按 `spring.cloud.bus.destination` 寻址），避免逐台调用。官方文档：[Spring Cloud Bus](https://cloud.spring.io/spring-cloud-bus/reference/html/index.html)。
3. **Config Server 侧的 push 触发**：`spring-cloud-config-monitor` 暴露 `/monitor`，接收 GitLab/GitHub 等 webhook → 通过 Bus 向客户端广播 `RefreshRemoteApplicationEvent`；客户端再结合 `spring.cloud.config.watch.enabled=true`（配合 `spring-cloud-config-monitor`）自动刷新。官方文档：[Push Notifications and Spring Cloud Bus](https://docs.springframework.org.cn/spring-cloud-config/reference/server/push-notifications-and-bus.html)。

### 1.3 Spring Boot 2.4+ 的 config data API

Spring Boot 2.4 引入 `spring.config.import` / `ConfigData` 体系：`ConfigDataLocationResolver` + `ConfigDataLoader` 在 Environment 构建期把外部配置源作为 config data 导入（`optional:` 前缀可容忍不可达）。官方文档：[Spring Cloud Config Client](https://docs.spring.io/spring-cloud-config/reference/4.2-SNAPSHOT/client.html)（含 `spring.config.import=configserver:http://host:8888`、`spring.cloud.config.uri`、`fail-fast`、`retry`、`discovery` 等）、[Spring Boot external-config 源码文档](https://github.com/spring-projects/spring-boot/blob/v3.5.10/spring-boot-project/spring-boot-dev_docs/src/dev_docs/antora/modules/reference/pages/features/external-config.adoc)。`spring-cloud-config-client`、`spring-cloud-vault`、`spring-cloud-kubernetes`、Nacos starter（3.x）都已迁移到 ConfigData；旧 bootstrap 机制仅通过 `spring-cloud-starter-bootstrap` 保留。**这也是 Defing Starter（路径 A）应采用的现代接入方式**。

---

## 2. spring-cloud-kubernetes：ConfigMap/Secret 消费机制

官方文档：[Using a ConfigMap PropertySource](https://docs.spring.io/spring-cloud-kubernetes/reference/3.1/property-source-config/configmap-propertysource.html)、[Reload namespace and label filtering](https://docs.spring.io/spring-cloud-kubernetes/reference/3.1/property-source-config/namespace-label-filtering.html)、[PropertySource Reload](https://docs.springframework.org.cn/spring-cloud-kubernetes/reference/property-source-config/propertysource-reload.html)、[Spring Cloud Kubernetes Configuration Watcher](https://docs.spring.io/spring-cloud-kubernetes/reference/spring-cloud-kubernetes-configuration-watcher.html)。

- **读取**：`ConfigMapPropertySourceLocator` 按"应用名 = ConfigMap 名"（或 `spring.cloud.kubernetes.config.name` / label 过滤）读取 ConfigMap，`SecretsPropertySourceLocator` 读取 Secret，均实现 `PropertySourceLocator`（ConfigData 化后为 `ConfigMapConfigDataLoader`/`SecretsConfigDataLoader`），直接进 Spring `Environment`。支持 `fail-fast`、按 profile 的后缀资源（`{name}-{profile}`）。
- **热更新（reload）**：`spring.cloud.kubernetes.reload.enabled=true`，策略三选一：
  - `refresh`：发布 `RefreshEvent`，只重建 `@RefreshScope` bean（最常用）；
  - `restart`：重启 Spring 上下文；
  - `shutdown`：关闭应用（配合编排拉起）。
  - 模式 `mode=event`（K8s watch 事件驱动，默认）或 `mode=polling`（周期轮询，`period` 可配），可分别开关 `monitoring-config-maps` / `monitoring-secrets`。
- **Config Watcher（独立组件）**：`spring-cloud-kubernetes-config-watcher` 是一个可独立部署的应用，监听某 namespace 下 ConfigMap/Secret 变化，然后通过 **Spring Cloud Bus** 或直接调用各应用 **`/actuator/refresh`** 把刷新广播出去——这正好解决了"多实例都要刷新"的问题。
- **零代码消费结论**：是的。Spring 应用只需加依赖 + 少量配置 + RBAC（get/list/watch），业务代码零改动；如果外部系统（如 Defing 同步控制器）把配置渲染进 ConfigMap/Secret，应用端完全不知道 Defing 的存在。局限：强依赖 K8s、ConfigMap 单条上限 1 MiB、Secret 只是 base64 混淆、跨集群/多 namespace 要分别处理、同步是"最终一致"。

---

## 3. Spring Cloud Alibaba Nacos Config：模型与监听

官方材料：[Nacos 概念（namespace/group/dataId）](https://nacos.io/zh-cn/dev_docs/concepts/)、[Spring Cloud Alibaba Nacos Config 快速开始](https://sca.aliyun.com/dev_docs/2023/user-guide/nacos/quick-start/)、社区整理 [Configuration DataId and Namespace Structure](https://deepwiki.com/alibaba/spring-cloud-alibaba/3.2-configuration-dataid-and-namespace-structure)。

- **三级模型**：`namespace`（租户/环境隔离，默认 public）→ `group`（分组，默认 `DEFAULT_GROUP`）→ `dataId`（单个配置文件，惯例为 `{prefix}-{spring.profiles.active}.{file-extension}`）。
- **关键配置项**：`spring.cloud.nacos.config.server-addr`、`namespace`、`group`、`file-extension`（properties/yaml）、`prefix`/`name`、`shared-configs`（共享配置）、`extension-configs`（扩展配置）、`refresh-enabled`（默认 true）、`enable-remote-sync-config`。
- **注解与刷新**：`@NacosValue(value="${key:default}", autoRefreshed=true)` 支持单值自动刷新；`@RefreshScope` + `@ConfigurationProperties` 也支持整类刷新。底层用 `ConfigService.addListener(dataId, group, Listener)` 注册监听。
- **监听机制（推送链路）**：Nacos 客户端对服务端做**长轮询**（v1 为 HTTP long-polling + UDP，v2 为 gRPC 长连接）获取"配置变更通知"，拿到通知后再拉取完整配置内容，由 starter 把变更发布为 Spring 的 `RefreshEvent` → `RefreshScope` 重建 bean。**这是"客户端监听 + 事件驱动刷新"的标准范式，Defing Starter（路径 A）完全可以照抄**。
- **Defing 模型 → Nacos 模型映射建议**（供路径 A 参考，不必真的引入 Nacos）：
  - `namespace` ← 项目（或按环境/租户拆分项目命名空间）；
  - `group` ← 分支（Defing 分支常按 dev/test/prod 命名，天然像 group 层）；
  - `dataId` ← 分组 + 渲染格式，如 `gateway.yaml` / `gateway.json` / `gateway.toml`；
  - 公共分组用 `shared-configs` 注入；
  - 即：**项目→namespace、分支→group、分组→dataId、item→配置键**，三/四层一一对应，用户心智一致。

---

## 4. Spring Cloud Consul Config / ZooKeeper Config：KV watch 模式

官方文档：[Distributed Configuration with Consul](https://docs.spring.io/spring-cloud-consul/reference/config.html)、[Distributed Configuration with Zookeeper](https://docs.spring.io/spring-cloud-zookeeper/reference/4.1/config.html)。

- **Consul**：KV 前缀 `config/`，键为 `config/{application}`、`config/{application}-{profile}`（含 `default-context=application` 的公共层），值支持 `KEY_VALUE|YAML|PROPERTIES`（`format` + `data-key`）。**Watch**：`spring.cloud.consul.config.watch.enabled=true`（默认）→ 客户端用 Consul **blocking query**（带 `index`+`wait` 的长连接）监听键变化 → 变化即发布 `RefreshEvent`。另有 `fail-fast`、`acknowledge`（消费确认避免重复推送）。
- **ZooKeeper**：根节点 `/config`（`spring.cloud.zookeeper.config.root`），键 `/{root}/{application}`、`/{root}/{application},{profile}`（`profileSeparator`、`defaultContext`）。**Watch**：`spring.cloud.zookeeper.config.watcher.enabled=true`（默认）→ `getData` 注册 ZK Watcher → `NodeDataChanged` 触发 `RefreshEvent`。
- **共性范式**：两者都是"客户端 `PropertySourceLocator` 加载 KV → 注册 watch → 变更时发布 `RefreshEvent` → `@RefreshScope` 重建"。**这正是 Defing Starter 的成熟参考模板**：Defing 的 SSE watch / gRPC 数据面替代 Consul 的 blocking query / ZK 的 Watcher，其余 Spring 侧逻辑（事件发布、RefreshScope、@ConfigurationProperties 重新绑定）完全复用 spring-cloud-context。

---

## 5. 集成路径对比（重点）

### 5.1 路径 A：Java SDK + Spring Boot Starter（客户端直连模式）

**工作原理**

1. SDK 核心：Defing HTTP/gRPC 客户端（拉取 + 长连接 watch），提供 `ConfigFetcher`、变更监听回调；
2. Starter 装配：实现 `ConfigDataLocationResolver`/`ConfigDataLoader`（Spring Boot 2.4+/3.x 正统方式），支持 `spring.config.import=defing:项目/分支/分组`，把渲染结果作为 `PropertySource` 注入 Environment；老版本可退化为 `PropertySourceLocator` + bootstrap；
3. 热更新：watch 回调 → 发布 `RefreshEvent`（spring-cloud-context）→ `@RefreshScope` + `@ConfigurationProperties` 重建；可选提供 `@DefingValue`（仿 `@NacosValue`）；
4. 健壮性：`fail-fast`、重试/退避、断线重连、本地缓存降级。

**优点**

- **watch 推送 100% 保留**：直接消费 Defing 的 SSE/gRPC watch，端到端延迟最低，可精确到"发布→通知→刷新"闭环；
- 无 K8s 依赖，任何部署形态（VM/容器/云上）可用；不引入中间组件；
- 项目/分支/分组模型可完整暴露给 Java 用户（`@DefingValue`、多分组 PropertySource 顺序、渲染格式选择）；
- 与现有 TS/Go/Python SDK 平行的第一方 Java SDK，后续可服务非 Spring 的 Java 应用。

**缺点**

- 需要维护 Java SDK + Starter + 版本矩阵（Boot 2.4/2.7/3.x、javax/jakarta 两套编译）与文档；
- 每个接入方都要升级依赖并做少量配置（不算零代码）；
- 热更新只覆盖 `@RefreshScope` bean，非托管代码（静态配置、第三方库读取点）仍需自行处理。

**工作量：M**（SDK 核心 S + Starter 装配与刷新 S + 双版本矩阵测试 M，合计 M）。参考：Nacos/Consul 客户端 starter 都是这个体量。

### 5.2 路径 B：Spring Cloud Config Server 作为网关/代理（Defing 实现 EnvironmentRepository）

**工作原理**

1. 部署一个 Spring Cloud Config Server 应用，`@Bean` 注册 `EnvironmentRepository`：把 `(application, profile, label)` 翻译成 Defing 查询（HTTP/gRPC），组装 `Environment` + `PropertySource`（按需渲染 yaml/json/properties），返回给 Config Server 的 `/application/{profile}/{label}` 端点；
2. 客户端零改动：任何 Spring Cloud 应用用官方 `spring-cloud-config-client` + `spring.config.import=configserver:http://...` 接入，`fail-fast`/重试/`@RefreshScope` 全部现成；
3. 热更新：Config Server 协议本身是**拉取式**的，需要额外链路——Defing 发布事件 → webhook/SSE 桥 → `spring-cloud-config-monitor` 的 `/monitor` → Spring Cloud Bus 广播 `RefreshRemoteApplicationEvent` → 客户端自动 `refresh`；否则只能手动 POST `/actuator/refresh` 或定时刷新。

**优点**

- **客户端零 SDK**：用官方成熟客户端，Java 生态工具链（bus、monitor、discovery、actuator、配置加密）全复用；
- 一个 Config Server 同时服务多种语言（协议是 HTTP+JSON）；
- 与"配置服务本身要治理/审计/灰度"的场景天然分层。

**缺点**

- 多一跳代理，多一个要运维的高可用组件（Config Server 自己也要集群化）；
- **watch 推送被明显削弱**：端到端刷新依赖 webhook→monitor→Bus 整条链路 + MQ（RabbitMQ/Kafka）基础设施，链路长、故障点多；无 MQ 时只能手动/轮询刷新；
- 模型映射有损：Defing 四层（项目/分支/分组/item）压进 Config Server 三元组（application/profile/label），多分组要么映射成多 profile（一次请求可带 `profile1,profile2`），要么多 label 需多次 `spring.config.import`，需要设计映射约定；
- label 语义（git 分支习惯）与 Defing 分支不完全对齐，容易踩坑。

**工作量：M**（EnvironmentRepository 实现 + 映射约定 + 部署 Config Server = M；若加 Bus/Monitor 推送链路，再加基础设施，可到 M+）。

### 5.3 路径 C：K8s 同步控制器 → ConfigMap/Secret + spring-cloud-kubernetes

**工作原理**

1. 独立控制器（Rust 二进制，用 K8s client 库）订阅 Defing 的 watch，把"某项目某分支某分组"渲染结果写入目标 namespace 的 ConfigMap/Secret（含删除、排序、版本注释等对账逻辑）；
2. Spring 应用只加 `spring-cloud-starter-kubernetes-client-all` + `spring.cloud.kubernetes.config.enabled=true` + `spring.cloud.kubernetes.reload.enabled=true`（策略 `refresh`）；
3. 刷新链：K8s watch → reload → `RefreshEvent`；多实例广播可再部署 `spring-cloud-kubernetes-config-watcher`（Bus 或调 `/actuator/refresh`）。

**优点**

- **Spring 侧真正零代码**：业务代码、客户端逻辑零改动，只加依赖和配置，RBAC 就绪即可；
- 配置统一落入 K8s 体系（kubectl 可见、可审计、可备份），与现有 ConfigMap 治理工具/Operator 生态兼容；
- Defing 只跟"一个控制器"对接，不需要逐应用兼容。

**缺点**

- **强依赖 K8s**：非 K8s 部署（本地开发、VM、裸机）不可用；
- ConfigMap 限制：单条 1 MiB（etcd 上限）、Secret 仅 base64、大量配置要分片管理；
- 同步是"最终一致"：Defing → 控制器 → ConfigMap → watcher → refresh 多级传递，延迟秒级且事件可能放大/丢序，复杂对账（删除、并发发布、回滚）要自己写；
- 控制器本身是又一个要保证高可用与正确性的分布式组件（比 Config Server 更重）。

**工作量：L**（控制器 + RBAC + 对账/同步语义 + 分片与容量处理 + 运维）。

### 5.4 对比小结

| 维度 | A SDK+Starter | B Config Server 代理 | C K8s 同步控制器 |
|---|---|---|---|
| 客户端改动 | 加依赖+少量配置 | 加官方依赖+配置 | 加依赖+配置（零业务代码） |
| watch 推送保留 | **完全保留（最优）** | 弱化（需 webhook+Bus+MQ） | 半保留（K8s 事件链，秒级） |
| 部署依赖 | 无 | Config Server + 可选 MQ | K8s |
| 模型保真度 | 完整（项目/分支/分组/item） | 有损（3 元组映射） | 分组粒度写入 ConfigMap |
| 工作量 | M | M | L |
| 适合场景 | 追求热更新体验、多平台 | 存量 Spring Cloud 应用平滑接入、多语言 | 纯 K8s 环境、零代码诉求 |

### 5.5 落地建议

以 **A 为主**（保留 Defing 推送优势），可选叠加 **B** 作为兼容层（让存量纯 Spring Cloud 应用/其他语言客户端零 SDK 接入，或作为灰度/迁移通道）；C 仅在"目标环境必然 K8s 且接受最终一致"时考虑。A 与 B 可共享同一套映射约定。

---

## 6. 契约与协议：Spring Cloud Config Server 协议是否值得兼容

Config Server 对外协议（客户端 `spring-cloud-config-client` 消费的正是它）：

- JSON 端点：`/{application}/{profile}[/{label}]`（`profile` 可逗号分隔多 profile），返回：
  `{name, profiles, label, version, state, propertySources:[{name, source:{key:value}}]}`；
- 文件端点：`/{application}-{profile}.{yml|properties|json}`、`/{label}/{application}-{profile}.yml` 等；
- 纯文本端点：`/{application}/{profile}/{label}/{path}`（Serving Plain Text，社区文档见 [serving-plain-text](https://spring-cloud-config.spring-doc.cn/dev_docs/3.1.6/S_3__serving_plain_text.en.html)）；
- 加密端点 `/encrypt`/`/decrypt`、健康检查（health = 各 backend 可达性）；社区还有 [Config Server OpenAPI 描述](https://raw.githubusercontent.com/api-evangelist/spring-cloud-config/refs/heads/main/openapi/spring-cloud-config-server-api.yml)。

**兼容性评估**

- **值得兼容**（若走路径 B）：协议简单、稳定、版本演进谨慎；官方客户端 `spring-cloud-config-client` 成熟且跨 Boot 2.x/3.x；一次实现可服务所有语言（不只 Java）；能直接复用 bus/monitor/discovery 生态。开销仅是"实现一个 EnvironmentRepository + 映射"。
- **但协议不承载推送语义**：它是"客户端主动拉取 + 手动/事件触发 refresh"模型，SSE/gRPC 的实时推送必须靠外部桥（webhook→/monitor→Bus）补，这正是路径 B 的天然短板。
- **若走路径 A**：无需兼容该协议，但可把 Config Server 协议作为"附加兼容端点"低成本提供（一个 EnvironmentRepository 而已），换取生态兼容性——即 A+B 组合。
- **映射约定建议**：`application=项目`；`profile=分支`（Defing 分支多按环境命名，天然对应 profile），`label=分组`（多分组用多次 `spring.config.import` 不同 label，或反向：`profile=分组`（逗号分隔一次取多组）、`label=分支`）。推荐以 **profile=分组、label=分支** 为主映射，一次请求取多分组，`item` 扁平化为 property key，渲染格式对应端点后缀。

---

## 7. 对 Defing 的落地建议（Roadmap 草案）

1. **v1（路径 A 最小闭环）**：Java SDK 核心（HTTP/gRPC 客户端 + SSE/gRPC watch + `ConfigFetcher`）+ Spring Boot Starter（ConfigData 加载 + `RefreshEvent` 发布 + `@DefingValue`），支持 `spring.config.import=defing:项目/分支/分组`。
2. **v1 配套**：映射约定文档（项目→namespace、分支→group、分组→dataId，见 §3）；版本矩阵（Boot 2.4/2.7/3.x）CI 构建与示例工程。
3. **v2（可选兼容层）**：实现 `EnvironmentRepository` 的 Config Server 兼容端点（路径 B），让存量 Spring Cloud 应用与多语言客户端零 SDK 接入；配合 webhook→`/monitor`→Bus 推送链路说明。
4. **v3（按需）**：若目标环境纯 K8s 且要零代码，评估路径 C 同步控制器（可复用 dev_docs/research-k8s-k3s-integration.md 的控制器设计结论）。
5. **治理**：热更新只覆盖 `@RefreshScope` bean 的边界要在文档中写清；`fail-fast`/重试/降级策略对齐 Nacos/Consul 客户端行为。

---

## 附录：参考 URL 总表（全部经 web_search 获得）

1. Custom Environment Repositories（Config Server 自定义 backend）：https://docs.spring.io/spring-cloud-config/reference/4.1/server/environment-repository/custom-enviroment-repository.html
2. Composite Environment Repositories（多 backend 合并）：https://docs.spring.io/spring-cloud-config/reference/4.2/server/environment-repository/composite-repositories.html
3. Git Backend（默认 backend 说明）：https://docs.spring.io/spring-cloud-config/reference/server/environment-repository/git-backend.html
4. Spring Cloud Config Client（4.2，spring.config.import 用法）：https://docs.spring.io/spring-cloud-config/reference/4.2-SNAPSHOT/client.html
5. Spring Cloud Config Client（4.1）：https://docs.spring.io/spring-cloud-config/reference/4.1/client.html
6. Push Notifications and Spring Cloud Bus（/monitor 与推送）：https://docs.springframework.org.cn/spring-cloud-config/reference/server/push-notifications-and-bus.html
7. custom-enviroment-repository.adoc（GitHub v4.2.4 源码文档）：https://github.com/spring-cloud/spring-cloud-config/blob/v4.2.4/dev_docs/modules/ROOT/pages/server/environment-repository/custom-enviroment-repository.adoc
8. Spring Boot external-config（GitHub antora v3.5.10，config data / spring.config.import）：https://github.com/spring-projects/spring-boot/blob/v3.5.10/spring-boot-project/spring-boot-dev_docs/src/dev_docs/antora/modules/reference/pages/features/external-config.adoc
9. ConfigData API 示例（spring-cloud-vault）：https://docs.springframework.org.cn/spring-cloud-vault/reference/config-data.html
10. Using a ConfigMap PropertySource（spring-cloud-kubernetes）：https://docs.spring.io/spring-cloud-kubernetes/reference/3.1/property-source-config/configmap-propertysource.html
11. PropertySource Reload（refresh/restart/shutdown 策略）：https://docs.springframework.org.cn/spring-cloud-kubernetes/reference/property-source-config/propertysource-reload.html
12. Spring Cloud Kubernetes Configuration Watcher：https://docs.spring.io/spring-cloud-kubernetes/reference/spring-cloud-kubernetes-configuration-watcher.html
13. Nacos 概念（namespace/group/dataId）：https://nacos.io/zh-cn/dev_docs/concepts/
14. Spring Cloud Alibaba Nacos Config 快速开始：https://sca.aliyun.com/dev_docs/2023/user-guide/nacos/quick-start/
15. Distributed Configuration with Consul（KV watch）：https://docs.spring.io/spring-cloud-consul/reference/config.html
16. Distributed Configuration with Zookeeper（Watcher）：https://docs.spring.io/spring-cloud-zookeeper/reference/4.1/config.html
17. Spring Cloud Bus（RefreshRemoteApplicationEvent 广播）：https://cloud.spring.io/spring-cloud-bus/reference/html/index.html
18. Serving Plain Text（Config Server 纯文本端点，中文镜像）：https://spring-cloud-config.spring-doc.cn/dev_docs/3.1.6/S_3__serving_plain_text.en.html
19. Config Server OpenAPI（社区整理）：https://raw.githubusercontent.com/api-evangelist/spring-cloud-config/refs/heads/main/openapi/spring-cloud-config-server-api.yml
