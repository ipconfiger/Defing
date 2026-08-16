//! wire 层脱敏工具（F1/F2 修复）：事件与 diff 出网前的 secret 密文掩码。
//!
//! 背景：HTTP SSE watch 与 branch_diff 曾直接把 `Value::Secret(Ciphertext)` 序列化出网，
//! 违反 design-v2 §7.6「数据面不解密 secret：脱敏」。本模块提供唯一掩码出口：
//! - [`masked_value`]：`Value::Secret(_)` → `Value::String("***")`（不含任何密文字段），
//!   其余值原样克隆（保持既有 wire 形状，零破坏）；
//! - [`mask_event_for_wire`]：对发布事件 changes 逐项掩码（watch 重放/实时共用）。
//!
//! 注：gRPC 数据面经 `grpc::value_to_proto` 独立脱敏（masked 标志），本模块服务 HTTP 面。

use crate::model::{DiffEntry, PublishEvent, Value};

/// secret 密文 → 掩码字符串；非 secret 原样克隆。
pub fn masked_value(v: &Value) -> Value {
    match v {
        Value::Secret(_) => Value::String("***".to_string()),
        other => other.clone(),
    }
}

/// 对 diff 条目掩码（secret 密文 → "***"）。
pub fn mask_diff_entry(d: &DiffEntry) -> DiffEntry {
    let mut d = d.clone();
    if let Some(nv) = &mut d.new_value {
        *nv = masked_value(nv);
    }
    d
}

/// 事件级脱敏：克隆事件并掩码全部 changes（重放与实时事件出网前统一调用）。
/// 不修改原事件（广播/日志仍保留密文，仅出网形状脱敏）。
pub fn mask_event_for_wire(e: &PublishEvent) -> PublishEvent {
    let mut e = e.clone();
    e.changes = e.changes.iter().map(mask_diff_entry).collect();
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BranchName, ChangeKind, Ciphertext, EventType, ProjectId};

    fn ct() -> Ciphertext {
        Ciphertext {
            enc: "aes-256-gcm".into(),
            v: 1,
            dek_v: 1,
            nonce: "bm9uY2U=".into(),
            ct: "Y3Q=".into(),
            edek: "ZWRlaw==".into(),
            edek_nonce: "ZW4=".into(),
        }
    }

    #[test]
    fn masked_value_strips_ciphertext() {
        let v = masked_value(&Value::Secret(ct()));
        assert_eq!(v, Value::String("***".into()));
    }

    #[test]
    fn masked_value_keeps_plain_types() {
        for v in [
            Value::String("x".into()),
            Value::Int(1),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Json("{\"a\":1}".into()),
            Value::Array(vec!["a".into()]),
        ] {
            assert_eq!(masked_value(&v), v);
        }
    }

    #[test]
    fn mask_event_removes_all_ciphertext() {
        let e = PublishEvent {
            project: ProjectId("p".into()),
            branch: BranchName("dev".into()),
            version: 3,
            ty: EventType::ValuePublish,
            structure_version: 1,
            comment: "c".into(),
            request_id: "r".into(),
            changes: vec![
                DiffEntry {
                    group: "g".into(),
                    key: "host".into(),
                    kind: ChangeKind::Upsert,
                    new_value: Some(Value::String("10.0.0.1".into())),
                },
                DiffEntry {
                    group: "g".into(),
                    key: "pass".into(),
                    kind: ChangeKind::Upsert,
                    new_value: Some(Value::Secret(ct())),
                },
            ],
        };
        let orig = e.clone();
        let masked = mask_event_for_wire(&e);
        // 原事件不受影响（广播/日志仍含密文）
        assert!(matches!(orig.changes[1].new_value, Some(Value::Secret(_))));
        // 掩码后无密文
        let ser = serde_json::to_string(&masked).unwrap();
        assert!(!ser.contains("edek"));
        assert!(!ser.contains("ciphertext"));
        assert!(!ser.contains("Y3Q="));
        assert!(ser.contains("***"));
        assert_eq!(masked.changes[0], orig.changes[0]);
    }
}
