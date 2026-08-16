# 开发计划：G4 灰度管理面 + Admin UI

> 依据：[design/g4-management.md](design/g4-management.md)（D29-D30 定稿）
> 目标：openapi 契约补全 + Admin UI 灰度操作面板 + api-surface 断言组（后端端点 G3 已就绪）。

## 任务分解

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| 1 | openapi 新增 4 灰度路径（gray-publish/promote/abort/status）+ ConfigSnapshot 加 gray/resolved_version | api/openapi.v1.yaml | check-contracts.sh 过 | | ✅ |
| 2 | UI index.html：项目详情内"灰度发布"卡（状态区 + 规则 textarea + 发布/转正/下量按钮） | admin/index.html | 浏览器实测 | | ✅ |
| 3 | UI app.js：loadGrayStatus/saveGrayPublish/doGrayPromote/doGrayAbort + 分支联动刷新（esc 转义 + data-act） | admin/app.js | 浏览器实测 | | ✅ |
| 4 | api-surface-test.sh 灰度断言组：发布/状态/数据面联动/转正/下量/审计 | scripts/api-surface-test.sh | 退出 0 | | ✅ |
| 5 | 全量回归：workspace 测试 + contract + clippy/fmt + 浏览器/脚本实测 | 命令行 | 达标 | | ✅ |
| 6 | 文档收尾：roadmap-p4.md G4 标记 + 审核记录 | docs | 完成 | | ✅ |

## 里程碑

- M1（1）：openapi 契约
- M2（2-3）：Admin UI 灰度面板
- M3（4-5）：api-surface 断言 + 回归
- M4（6）：文档

## 关键纪律

- **D-CSP**：无 inline script/onclick；`esc()` 转义所有回显文本；
- **审计**：灰度操作走既有 audit action（G3 已实现），api-surface 断言覆盖；
- **不碰状态机/wire**：G4 纯管理面（端点 G3 已就绪，UI 只调端点）。

## 风险

- 规则 JSON 非法 → 前端 parse + 服务端 validate 双保险；
- openapi 漂移 → contract 检查 + api-surface 对拍。
