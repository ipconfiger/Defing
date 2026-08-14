//! diff 计算（同结构按 key，O(变更项)）。

use crate::model::{ChangeKind, DiffEntry, SnapshotMap};

/// 计算 old → new 的变更列表（按 BTreeMap 有序输出，确定性）。
/// secret 值以密文比较（不解密）。
pub fn compute_diff(old: &SnapshotMap, new: &SnapshotMap) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    // upsert / 变更：遍历 new
    for (group, items) in new {
        for (key, value) in items {
            let old_val = old.get(group).and_then(|m| m.get(key));
            if old_val != Some(value) {
                out.push(DiffEntry {
                    group: group.clone(),
                    key: key.clone(),
                    kind: ChangeKind::Upsert,
                    new_value: Some(value.clone()),
                });
            }
        }
    }
    // delete：new 中没有而 old 有
    for (group, items) in old {
        for key in items.keys() {
            if !new.get(group).is_some_and(|m| m.contains_key(key)) {
                out.push(DiffEntry {
                    group: group.clone(),
                    key: key.clone(),
                    kind: ChangeKind::Delete,
                    new_value: None,
                });
            }
        }
    }
    out
}

/// 将 diff 应用到 base，得到应用后的快照（测试/回放用）。
pub fn apply_diff(base: &SnapshotMap, diff: &[DiffEntry]) -> SnapshotMap {
    let mut out = base.clone();
    for d in diff {
        match d.kind {
            ChangeKind::Upsert => {
                out.entry(d.group.clone())
                    .or_default()
                    .insert(d.key.clone(), d.new_value.clone().unwrap());
            }
            ChangeKind::Delete => {
                if let Some(m) = out.get_mut(&d.group) {
                    m.remove(&d.key);
                    if m.is_empty() {
                        out.remove(&d.group);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;

    fn map(pairs: &[(&str, &str, &str)]) -> SnapshotMap {
        let mut m = SnapshotMap::new();
        for (g, k, v) in pairs {
            m.entry((*g).to_string())
                .or_default()
                .insert((*k).to_string(), Value::String((*v).to_string()));
        }
        m
    }

    #[test]
    fn upsert_change_delete() {
        let old = map(&[("redis", "host", "10.0.0.1"), ("redis", "port", "6379")]);
        let new = map(&[("redis", "host", "10.0.0.9"), ("redis", "port", "6380")]);
        // 值变更 + 新增 db 组
        let new2 = {
            let mut m = new;
            m.entry("db".into())
                .or_default()
                .insert("user".into(), Value::String("root".into()));
            m
        };
        let diff = compute_diff(&old, &new2);
        assert_eq!(diff.len(), 3); // host, port, db/user
        assert_eq!(apply_diff(&old, &diff), new2);
    }

    #[test]
    fn delete_when_absent() {
        let old = map(&[("a", "x", "1"), ("a", "y", "2")]);
        let new = map(&[("a", "x", "1")]);
        let diff = compute_diff(&old, &new);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].kind, ChangeKind::Delete);
        assert_eq!(diff[0].key, "y");
        assert_eq!(apply_diff(&old, &diff), new);
    }

    #[test]
    fn no_diff_when_equal() {
        let m = map(&[("a", "x", "1")]);
        assert!(compute_diff(&m, &m).is_empty());
    }

    #[test]
    fn secret_compared_by_ciphertext() {
        // secret 以密文比较：相同密文视为未变更（解密比较由上层按需做）
        let ct = crate::model::Ciphertext {
            enc: "aes-256-gcm".into(),
            v: 1,
            dek_v: 1,
            nonce: "n".into(),
            ct: "c".into(),
            edek: "e".into(),
            edek_nonce: "en".into(),
        };
        let mut old = SnapshotMap::new();
        old.insert(
            "auth".into(),
            [("pwd".into(), Value::Secret(ct.clone()))].into(),
        );
        assert!(compute_diff(&old, &old).is_empty());
    }
}
