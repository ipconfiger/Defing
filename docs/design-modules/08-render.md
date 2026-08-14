# 模块 08 —— 多格式渲染（dsh-render）

> 依据：design-v2 §8、design-v3 §5（RND-001）
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：版本物化（引用解析）→ 规范化 IR → YAML/TOML/JSON 渲染；TOML 表达力约束；
  三格式语义等价性校验工具。
- 不做：加密（secret 值由上层决定解密或掩码后传入）；网络。

## 2. 数据流

```
(项目, 分支, 版本) → materialize(引用解析, design-v2 §8.3) → IR（map<group, map<key, Value>>）
→ 渲染器(YAML/TOML/JSON) → 字符串（按格式）
等价性校验（测试用）：IR → 三格式 → 解析 → 规范化比较
```

## 3. 渲染器接口

```
pub struct Renderer { /* serde 配置 */ }
impl Renderer {
    pub fn render(&self, ir: &Ir, format: Format) -> Result<String>;
    // Format: Yaml | Toml | Json
    pub fn render_bundle(&self, ir: &Ir) -> Result<Bundle>;   // 三格式打包（zip 条目）
}
pub struct Ir { pub groups: BTreeMap<String, BTreeMap<String, Value>> }   // 物化后、有序
```

## 4. TOML 表达力约束（design-v2 §8.2，渲染前校验）
- 分组名/键：合法 TOML 标识符或引号串（校验失败 → ERR_VALIDATION 明细）。
- 顶层：分组 → 顶层表；空分组输出注释占位。
- json 类型值：输出为内联表/数组，或字符串（规则：先尝试结构化，失败降级字符串，文档化）。
- 无 null：secret 未填 → 注释 `# key: (unset)`。

## 5. secret 处理（两条路径，design-v2 §8.2）
- SDK/应用拉取（GetConfig 数据面）：解密后的真实值（模块 07 解密，仅出站瞬间）。
- 管理面渲染/导出：默认掩码（模块 07 mask）；reveal=true 需会话 + 审计。

## 6. 引用解析（materialize）
- 输入：版本快照（密文 secret 已随版本存储）、RefBinding 索引。
- 输出：IR（共享值已物化写入）；解析失败 → ERR_VALIDATION；环 → ERR_CYCLE_REF（模块 01 预检）。
- 注意：历史版本已物化（版本自包含），只有草稿/活动版本渲染才需要实时解析。

## 7. 等价性校验工具（测试/CI 用）
- `assert_equivalent(ir)`：三格式 → 各自解析 → 规范化（键排序/数值规范化）→ 相等。
- 属性测试：随机 IR 生成器（合法 + 边界：空分组、深层嵌套 json、unicode 键）。

## 8. 测试要点
- RND-001 随机 IR 三格式语义等价（属性测试）｜ TOML 约束违规报错
- 文件名/打包规则（design-v2 §8.2）：{project}-{branch}-v{version}.{ext}

## 9. 任务清单
□ Ir 类型与物化（引用解析） □ YAML/TOML/JSON 渲染器 □ TOML 约束校验
□ secret 两路径（解密/掩码） □ 等价性校验工具 + 随机 IR 生成器
□ 渲染端点接入（模块 05） □ RND-001 属性测试
