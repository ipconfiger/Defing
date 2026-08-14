# 模块 12 —— 三语言 SDK（TS / Go / Python）

> 依据：design-v2 §10、design-v3 §3/§7、proto/config.v1.proto
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 公共行为（三端一致，契约测试覆盖）
- 端点池 + failover：指数退避（500ms→30s + 抖动）；ERR_LEADER_REDIRECT 跟随 leader_hint 并缓存。
- 订阅 (项目, 分支)：首连拉全量（GetConfig）→ 事件增量更新本地缓存 → listener 回调（版本严格递增）。
- 断线续传：重连携带 after_version；ERR_VERSION_PRUNED / snapshot_required → 重拉全量。
- 重试矩阵（design-v3 §7 全表）：网络错误重试；NOT_FOUND 不重试；管理面写操作配幂等键。

## 2. 实现要点（对齐 design-v3 §3 签名）

| 语言 | 运行时 | 关键点 |
|------|--------|--------|
| TS | 浏览器（SSE/WebSocket）+ Node（grpc-js） | 双运行时适配层；类型化 WatchEvent；token 注入 |
| Go | grpc-go | goroutine 扇出；context 贯穿；sync.RWMutex 缓存；Watch 阻塞直至 ctx 取消 |
| Python | grpc.aio | async listener；asyncio 任务；线程安全缓存（单线程 + 锁） |

## 3. 错误类型

```
// 三端等价定义
ConfigError { code: string; message: string; leader_hint?: string }
// 内部：端点切换/退避对 listener 透明
```

## 4. 本地缓存与一致性
- 缓存结构：project → branch → { version, structure_version, groups }。
- 更新规则：仅按事件版本号顺序应用；跳号（重放后）→ 重拉全量兜底。

## 5. 契约测试（与 dsh-testkit mock 服务联跑）
- 用例：failover、leader 重定向、watch 顺序、断线重放、VERSION_PRUNED 重置、
  慢消费者恢复、secret 脱敏（masked 标记）、幂等重试、多格式获取（渲染 URL）。
- 同一套 golden 数据三语言各跑一遍，结果对拍。

## 6. 发布与版本管理
- 包名：TS `@defing/config-client`、Go `github.com/defing/config-go`、Python `defing-config`；
  语义化版本与 proto major 同步（v1.x）。

## 7. 任务清单
□ proto 代码生成接入（buf） □ 端点池 + failover + 退避 □ get/getItem
□ watch（缓存/事件/续传/重拉） □ 错误类型与重试矩阵 □ 双运行时（TS）
□ 契约测试接入（三语言对拍） □ 包发布流水线（npm/go/pypi，CI）
