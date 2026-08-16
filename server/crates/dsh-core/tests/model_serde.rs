//! 数据模型序列化测试：与 schema/storage.v1.schema.json 的语义对齐（golden 对拍）。

use dsh_core::model::*;

#[test]
fn value_roundtrip_all_types() {
    let values = vec![
        Value::String("hello".into()),
        Value::Int(42),
        Value::Float(1.25),
        Value::Bool(true),
        Value::Json("{\"a\":1}".into()),
        Value::Array(vec!["x".into(), "y".into()]),
        Value::Secret(Ciphertext {
            enc: "aes-256-gcm".into(),
            v: 1,
            dek_v: 3,
            nonce: "MTIzNDU2Nzg5MDEy".into(),
            ct: "Y3Q=".into(),
            edek: "ZWRlaw==".into(),
            edek_nonce: "MTIzNDU2Nzg5MDEy".into(),
        }),
    ];
    for v in values {
        let json = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back, "roundtrip failed for {json}");
    }
}

#[test]
fn structure_golden_matches_schema_shape() {
    // 与 schema/storage.v1.schema.json 的 Structure/ItemDef 语义对齐
    let structure = Structure {
        version: 1,
        groups: vec![GroupDef {
            name: "redis".into(),
            items: vec![
                ItemDef {
                    key: "host".into(),
                    ty: ValueType::String,
                    required: true,
                    secret: false,
                    validate: None,
                },
                ItemDef {
                    key: "password".into(),
                    ty: ValueType::Secret,
                    required: false,
                    secret: true,
                    validate: None,
                },
            ],
        }],
    };
    let json = serde_json::to_string_pretty(&structure).unwrap();
    let back: Structure = serde_json::from_str(&json).unwrap();
    assert_eq!(structure, back);
    // 类型字段序列化为 "type"（与契约对齐），值为 lowercase
    assert!(json.contains("\"type\": \"secret\""));
    assert!(json.contains("\"required\": true"));
}

#[test]
fn branch_state_with_draft() {
    let mut state = BranchState::new(1);
    state.value_draft.insert(
        "redis".into(),
        [(
            "host".into(),
            DraftValue {
                value: Value::String("127.0.0.1".into()),
                updated_at: 1000,
            },
        )]
        .into(),
    );
    let json = serde_json::to_string(&state).unwrap();
    let back: BranchState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, back);
    assert_eq!(
        back.value_draft["redis"]["host"].value,
        Value::String("127.0.0.1".into())
    );
}

#[test]
fn version_record_kind_serde() {
    let v = VersionRecord {
        no: 12,
        structure_version: 3,
        created_at: 123,
        operator: "admin".into(),
        comment: "fix".into(),
        rollback_of: Some(10),
        kind: VersionKind::Diff,
        snapshot_ref: None,
        diff_ref: Some("d".into()),
        event_ty: Some(EventType::Rollback),
    };
    let json = serde_json::to_string(&v).unwrap();
    let back: VersionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(v, back);
}

#[test]
fn publish_event_serde() {
    let e = PublishEvent {
        project: "p".into(),
        branch: "prod".into(),
        version: 12,
        ty: EventType::ValuePublish,
        structure_version: 3,
        comment: "c".into(),
        request_id: "r1".into(),
        changes: vec![DiffEntry {
            group: "g".into(),
            key: "k".into(),
            kind: ChangeKind::Upsert,
            new_value: Some(Value::Int(1)),
        }],
    };
    let json = serde_json::to_string(&e).unwrap();
    let back: PublishEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(e, back);
    assert!(json.contains("\"ty\":\"value_publish\"")); // 紧凑 JSON 无空格
}
