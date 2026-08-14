# 模块 09 —— Admin UI（前端）

> 依据：design-v2 §9、api/openapi.v1.yaml
> 版本：v1.0 ｜ 状态：开发就绪（页面线框见路线图，先按本规格实现功能）

## 1. 技术栈与构建
- React + TypeScript + Vite；产物由 rust-embed 编入二进制（模块 05 /admin 静态 + SPA fallback）。
- API 客户端：由 openapi.v1.yaml 生成（openapi-typescript + 手写薄封装）。
- 构建：CI 中 `vite build` → 哈希固定 → 内嵌；体积基准 ≤5MB；无外链资源。

## 2. 页面与路由

| 路由 | 页面 | 关键交互 |
|------|------|----------|
| /login | 登录 | 密码 + device_id；单会话被拒提示 + 强制下线按钮（I7） |
| /projects | 项目列表 | 创建/删除（force 确认） |
| /projects/:p | 项目详情 | 分支 Tab；结构入口 |
| /projects/:p/branches/:b | 分支编辑 | 树形值编辑；待发布变更视图；发布确认（校验结果+影响预览）；版本历史+回滚确认 |
| /projects/:p/structure | 结构编辑 | 分组/item 管理；发布影响预览（波及分支） |
| /projects/:p/diff | 分支对比 | 并排 diff + promote（目标草稿已改项 skipped 提示） |
| /shared | 共享库 | CRUD + 引用绑定 + 发布级联预览 |
| /audit | 审计 | 过滤查询 |
| /settings | 设置 | 主密钥状态、保留策略、成员、强制下线 |

## 3. 状态管理与数据流
- 本地状态：项目/分支/草稿（服务端为准，写后重新拉取）；watch 不用于管理面（管理面轮询 + 乐观更新）。
- 发布流程：点发布 → 服务端校验 → 成功返回 version/changes → 跳版本历史。
- 单会话：401/409 处理 → 提示重新登录或被顶替。

## 4. 安全要求
- CSP（default-src 'self'）、无内联脚本（Vite 构建产出）、XSS 防护（不渲染 raw HTML）。
- 初始凭证引导：must_change_password → 强制改密页（design-v2 §9.3）。

## 5. 组件清单（首批）
ValueEditor（按类型：string/int/bool/json/array/secret 掩码开关）、TreeNav、DiffViewer、
PublishConfirmModal、VersionTimeline、RollbackConfirmModal、CascadePreviewModal、AuditTable。

## 6. 测试要点
- 组件测试（Vitest + Testing Library）；E2E（Playwright）：登录→改草稿→发布→回滚 全流程；
- 单会话 E2E：第二个标签页登录被拒。

## 7. 任务清单
□ Vite 脚手架 + openapi 客户端生成 □ 路由骨架 □ 登录/单会话流
□ 分支编辑（树形+草稿+发布确认） □ 版本历史+回滚 □ diff+promote
□ 结构编辑+发布预览 □ 共享库+级联预览 □ 审计页 □ 改密引导
□ 内嵌构建（rust-embed）□ E2E 主流程
