# 模块 04 —— 发布引擎（dsh-publish）

> 依据：design-v2 §4、design-v3 §2.2/§2.5/§2.6/§4.3
> 版本：v1.0 ｜ 状态：开发就绪

## 1. 职责与边界
- 职责：Command 中发布相关命令的 apply 实现（Publish/PublishStructure/Rollback/SharedPublish/Promote 的语义），
  幂等（I10）、完整性校验、diff 生成、版本写入、事件产出。
- 不做：Raft 网络、事件扇出（产出事件由模块 06 消费）、加密算法（secret 值以密文透传，模块 07）。

## 2. 依赖接口（来自其他模块）
- dsh-core：Command/ApplyOutcome、BranchState、VersionRecord、Validator、compute_diff、键构造。
- dsh-crypto：Cipher（仅用于校验时比较密文？不需要——比较用密文 bytes）。
- 注入：`now_ms`、`policy: PublishPolicy`（block/warn）、`cascade: CascadeMode`（auto/manual）。

## 3. apply_publish（分支值发布，对应 design-v3 §2.2）

```
fn apply_publish(cmd: Publish, ctx: &mut Ctx) -> Result<Vec<PublishEvent>> {
    let st = load::<BranchState>(state_key(pid, branch))?;

    // 幂等（I10）：同 request_id 直接返回原结果
    if st.last_request_id.as_deref() == Some(&cmd.request_id) {
        return Ok(vec![]);   // 已在 apply 中产出过事件，幂等重放不重复
    }
    if st.value_draft.is_empty() { return Err(no_draft()); }

    // 1) 校验（required/类型/规则/引用）
    let errs = Validator::validate_publish(&st.value_draft, &structure)?;
    if !errs.is_empty() {
        if policy == Block { return Err(publish_blocked(errs)); }
        // warn 模式继续（detail 记录警告）
    }
    // 2) 物化引用（模块 08 的引用解析；失败 → validation）
    let resolved = materialize_refs(&st.value_draft, &refs)?;
    // 3) diff
    let old = snapshot_of(&st, active_version)?;
    let diff = dsh_core::compute_diff(&old, &resolved);
    // 4) 写版本（full/diff 按 checkpoint 规则）
    let vno = st.active_version + 1;
    write_version(pid, branch, vno, resolved, diff, kind, operator, comment)?;
    // 5) 推进指针 + 清空草稿 + 幂等记录
    st.active_version = vno; st.value_draft.clear(); st.last_request_id = Some(cmd.request_id);
    save(&st)?;
    // 6) 产出事件（确定性副作用）
    Ok(vec![PublishEvent { version: vno, ty: ValuePublish, structure_version,
                            comment, request_id, changes: diff }])
}
```

## 4. apply_publish_structure（结构发布，全分支同时生效，I3/I5）

```
fn apply_publish_structure(cmd, ctx) -> Result<Vec<PublishEvent>> {
    let draft = load::<StructureDraft>(struct_draft_key(pid))?;
    if draft.base_version != structure.version { return Err(conflict("base_version 不匹配")); }
    let errs = Validator::validate_structure(&draft.groups)?;   // 含 TOML 约束
    if !errs.is_empty() { return Err(publish_blocked(errs)); }
    // 写新结构；对每个分支：版本 +1（值不变，diff=结构变化）；被删 item 清草稿值（D14）
    let mut events = vec![];
    for branch in load_branches(pid)? {
        let vno = branch.active_version + 1;
        write_version(pid, branch, vno, /* 同值 */, structure_diff, kind=diff, ...)?;
        branch.active_version = vno;
        prune_deleted_draft_values(&mut branch.value_draft, &draft.groups)?;  // D14
        save(&branch)?;
        events.push(PublishEvent { ty: StructurePublish, ... });
    }
    save_structure(new_structure)?;
    Ok(events)
}
```

## 5. apply_rollback（I6/I9）

```
fn apply_rollback(cmd, ctx) -> Result<Vec<PublishEvent>> {
    if st.last_request_id == cmd.request_id { return Ok(vec![]); }   // 幂等
    let snap = load_version_snapshot(pid, branch, cmd.to_version)?;   // checkpoint 重建
    let vno = st.active_version + 1;
    write_version(pid, branch, vno, snap, compute_diff(active, snap), kind, rollback_of=to_version)?;
    st.active_version = vno; st.last_request_id = Some(cmd.request_id);
    save(&st)?;
    Ok(vec![PublishEvent { ty: Rollback, ... }])
}
```

## 6. apply_shared_publish（D7/D15，对应 design-v3 §2.6）

```
fn apply_shared_publish(cmd, ctx) -> Result<Vec<PublishEvent>> {
    // 校验共享草稿（含环检测）
    let changed = shared_draft.changed_keys();
    write_shared_version(new_version)?;
    if cascade == Auto {
        // 原子：同一 apply 内完成；任一步失败 → 整体 Err（Raft 提案失败 = 无部分生效）
        for (pid, branch) in find_refs(&changed)? {     // 反查 idx/ref
            let new_vals = resolve_shared(changed, pid, branch)?;
            let vno = branch.active_version + 1;
            write_version(pid, branch, vno, new_vals, diff, kind=diff, ...)?;
            branch.active_version = vno; save(&branch)?;
            events.push(PublishEvent { ty: SharedCascade, ... });
        }
    }
    Ok(events)
}
```

## 7. apply_promote（D13，写目标分支草稿，不发布）

```
fn apply_promote(cmd, ctx) -> Result<Vec<PublishEvent>> {
    let from_snap = snapshot_of(from_branch)?;          // 源活动版本
    for (group, key) in selected_items {
        if to_draft 中该 item 已本地修改 && !force { skipped.push; continue; }
        to_draft[group][key] = from_snap[group][key].clone(); applied.push;
        if from_snap 无值 { missing_from.push; }
    }
    save(to_branch)?;   // 只写草稿；发布由后续 Publish 完成
    Ok(vec![])          // 不产生 SDK 事件（I4）
}
```

## 8. 版本存储与 checkpoint（D3）
- 写入策略：vno 为 checkpoint 倍数（每 100）或首次 → full（快照）；否则 diff。
- 读取历史：最近 full 起应用 diff 序列；`snapshot_of(branch, version)` 封装。

## 9. 完整性校验与限额（复用 dsh-core Validator；限额在 draft_update 时校验）

## 10. 测试要点（对应 design-v3 §5）
- PUB-001 发布版本+1/指针推进/草稿清空 ｜ PUB-002 幂等重复 ｜ PUB-003 block 不产版本
- PUB-004 回滚=新版本(rollback_of) ｜ PUB-005 结构发布全分支推进
- SHR-001 级联 ｜ SHR-002 原子性（注入失败点）｜ SHR-003 环拒绝

## 11. 任务清单
□ apply_publish □ apply_publish_structure（含 D14 草稿清理） □ apply_rollback
□ apply_shared_publish（auto/manual） □ apply_promote（D13） □ 版本写入与 checkpoint
□ 幂等（last_request_id） □ 单元测试 PUB-001~005、SHR-001~003
