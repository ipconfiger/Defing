# 模块 13 —— 测试与 CI

> 依据：design-v2 §14、design-v3 §5/§8、模块 00 约定
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 测试金字塔（按数量/速度）

| 层 | 工具 | 内容 |
|----|------|------|
| 单元 | nextest（Rust）/ vitest（前端） | 模块规格中的任务清单测试 |
| 契约 | buf lint / swagger-cli / jsonschema 校验 | proto、openapi、storage schema 三方 lint（design-v3 §8） |
| 集成 | dsh-testkit | 发布/级联/回滚/幂等/单会话（模块 04/05） |
| Raft/混沌 | TestCluster（进程内→进程级）+ chaos 脚本 | RAFT-001~004、分区/丢包/kill |
| SDK 契约 | 三语言 vs mock 服务 | 模块 12 §5 |
| 等价性 | proptest | RND-001（随机 IR → 三格式） |
| E2E | Playwright | Admin UI 主流程（模块 09） |
| 基准 | criterion（Rust）/ ghz / k6 | 写 QPS ≥10k、watch ≥10k、发布→SDK ≤1s |

## 2. CI 流水线（分支：main 合并前必须全绿）

```
stage 1 lint:   rustfmt --check / clippy -D warnings / buf lint / openapi validate / cargo deny
stage 2 unit:   nextest（并行分片）
stage 3 contract: 契约 golden 对拍（proto ↔ openapi ↔ schema ↔ 内部模型）
stage 4 integ:  dsh-testkit 集成（发布/级联/回滚/幂等/单会话）
stage 5 raft:   3 节点集群测试（RAF-001~004）
stage 6 sdk:    三语言契约测试（对拍）
stage 7 e2e:    Playwright（UI 主流程）
stage 8 bench:  基准冒烟（结果归档，不 gate 合并，超阈值报警）
stage 9 release: 构建静态二进制 + 前端内嵌 + SBOM + 镜像推送
```

## 3. 关键测试基建（dsh-testkit）
- `TestCluster`：进程内多 Raft（单进程多实例，测试快）；进程级（真实网络）用于混沌。
- `MockRaft`：无网络的内存 apply 通道（core/publish 单测用）。
- `ContractServer`：golden 协议 mock（SDK 契约测试用）。
- `Golden`：storage schema 序列化样例（JSON），三方对拍。

## 4. 不变量门禁（design-v2 §18.2）
- 每次合并前跑不变量测试集：I1~I10 对应测试（模块 01~07 规格中列出的用例 ID）。

## 5. 任务清单
□ CI 骨架（stage 1~9） □ TestCluster（进程内） □ MockRaft □ ContractServer + golden
□ 不变量门禁（I1~I10） □ 混沌脚本（分区/kill） □ 基准场景脚本
