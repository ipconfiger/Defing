# Defing 使用教程站点

多页 **Jekyll** 站点的使用教程：项目介绍 + 分章节操作指南（重点 Admin UI），每章配**真实实例截图**（测试实例 `http://172.16.48.71:18384`，演示项目 `horizon-compile`）。

## 启用 GitHub Pages

1. 进入仓库 **Settings → Pages**
2. **Build and deployment → Source** 选择 **Deploy from a branch**
3. **Branch** 选默认分支（`main`），**目录**选择 **`/tutorial`**
4. Save 后等待构建完成（约 1 分钟），站点即发布于 `https://<用户名>.github.io/<仓库名>/`

> 无需任何额外配置：`_config.yml` 使用 GitHub Pages 内置的 Jekyll（自定义布局，零主题依赖），站点内为相对链接 / 相对图片路径，从子目录发布也能正常工作。

## 本地预览

```bash
cd tutorial
bundle init
bundle add jekyll
bundle exec jekyll serve
# 打开 http://127.0.0.1:4000
```

## 目录结构

```text
tutorial/
├── _config.yml          # Jekyll 配置（标题 / 导航章节）
├── _layouts/
│   └── default.html     # 站点布局（侧边导航 + 正文 + 上一章/下一章）
├── assets/images/       # 教程截图（Playwright 对测试实例实拍）
├── index.md             # 首页：项目介绍
├── quickstart.md        # 快速开始（10 分钟走完核心链路）
└── 01-install.md … 09-admin.md   # 分章节教程
```

## 章节

| 章节 | 内容 |
|---|---|
| 快速开始 | 登录 → 建项目 → 结构 → 填值发布 → SDK/curl 读取 |
| 01 部署与启动 | 单机 / 集群 / 参数 / 数据面令牌 |
| 02 项目与分支 | 项目 / 分支管理、值提升 |
| 03 结构配置与共享引用 | 分组 / 配置项类型 / 共享引用 |
| 04 草稿与发布 | 草稿编辑、校验、发布、版本与回滚 |
| 05 灰度发布 | 标签 / IP / 百分比灰度、转正 / 下量 |
| 06 共享库 | 跨项目复用与自动级联 |
| 07 访问令牌与 SDK | 令牌管理 + 三语言 SDK |
| 08 构建脚本取值 | curl 拉配置（yaml/json/toml/env） |
| 09 管理员与审计 | PA 账号、审计日志、集群节点 |

## 重新生成截图

测试实例界面变化后可重拍：登录测试实例后对关键界面执行全页截图（Playwright + 系统 Chrome），覆盖 `assets/images/` 下同名文件。
