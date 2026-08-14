//! 校验器（模块 01 §5）：item 值校验、发布必填校验、结构约束、限额。

use crate::limits::*;
use crate::model::{DraftValue, ItemDef, Structure, Value, ValueType};
use std::collections::BTreeMap;

/// 校验单个 item 值是否符合定义。返回错误列表（空 = 通过）。
pub fn validate_value(def: &ItemDef, value: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    match (def.ty, value) {
        (ValueType::String, Value::String(s)) => {
            if s.len() > MAX_VALUE_BYTES {
                errs.push(format!("{}: string too long", def.key));
            }
        }
        (ValueType::Int, Value::Int(_)) => {}
        (ValueType::Float, Value::Float(_)) => {}
        (ValueType::Bool, Value::Bool(_)) => {}
        (ValueType::Json, Value::Json(s)) => {
            if serde_json::from_str::<serde_json::Value>(s).is_err() {
                errs.push(format!("{}: invalid json", def.key));
            }
        }
        (ValueType::Array, Value::Array(items)) => {
            if items.iter().any(|i| i.len() > MAX_ARRAY_ELEMENT_BYTES) {
                errs.push(format!("{}: array element too long", def.key));
            }
        }
        (ValueType::Secret, Value::Secret(_)) => {}
        _ => errs.push(format!(
            "{}: type mismatch (expected {:?})",
            def.key, def.ty
        )),
    }
    errs
}

/// 发布前校验：必填项在草稿中是否有值 + 草稿值类型合法。
/// 返回错误列表（空 = 通过）。
pub fn validate_publish(
    draft: &BTreeMap<String, BTreeMap<String, DraftValue>>,
    structure: &Structure,
) -> Vec<String> {
    let mut errs = Vec::new();
    for g in &structure.groups {
        for item in &g.items {
            let has = draft
                .get(&g.name)
                .and_then(|m| m.get(&item.key))
                .map(|dv| &dv.value);
            if item.required && has.is_none() {
                errs.push(format!("{}/{}: required but unset", g.name, item.key));
                continue;
            }
            if let Some(v) = has {
                errs.extend(validate_value(item, v));
            }
        }
    }
    errs
}

/// 结构约束：分组/item 名唯一、key 长度、分组数/item 数限额。
pub fn validate_structure(structure: &Structure) -> Vec<String> {
    let mut errs = Vec::new();
    if structure.groups.len() > MAX_GROUPS_PER_PROJECT {
        errs.push(format!("too many groups (max {MAX_GROUPS_PER_PROJECT})"));
    }
    let mut seen_groups = std::collections::HashSet::new();
    let mut total_items = 0usize;
    for g in &structure.groups {
        if g.name.len() > MAX_GROUP_NAME_BYTES || g.name.is_empty() {
            errs.push("invalid group name length".into());
        }
        if !seen_groups.insert(g.name.clone()) {
            errs.push(format!("duplicate group: {}", g.name));
        }
        let mut seen = std::collections::HashSet::new();
        for item in &g.items {
            if item.key.len() > MAX_KEY_BYTES || item.key.is_empty() {
                errs.push(format!("{}: invalid key length", item.key));
            }
            if !seen.insert(item.key.clone()) {
                errs.push(format!("{}/{}: duplicate item key", g.name, item.key));
            }
            if item.secret && item.ty != ValueType::Secret {
                errs.push(format!(
                    "{}/{}: secret flag requires secret type",
                    g.name, item.key
                ));
            }
        }
        total_items += g.items.len();
    }
    if total_items > MAX_ITEMS_PER_PROJECT {
        errs.push(format!("too many items (max {MAX_ITEMS_PER_PROJECT})"));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DraftValue, GroupDef, ItemDef};

    fn str_def(key: &str, required: bool) -> ItemDef {
        ItemDef {
            key: key.into(),
            ty: ValueType::String,
            required,
            secret: false,
            validate: None,
        }
    }

    #[test]
    fn type_mismatch() {
        let def = str_def("port", false);
        assert!(!validate_value(&def, &Value::Int(8080)).is_empty());
        assert!(validate_value(&def, &Value::String("8080".into())).is_empty());
    }

    #[test]
    fn json_validity() {
        let def = ItemDef {
            key: "cfg".into(),
            ty: ValueType::Json,
            required: false,
            secret: false,
            validate: None,
        };
        assert!(validate_value(&def, &Value::Json("{bad".into())).len() == 1);
        assert!(validate_value(&def, &Value::Json("{\"a\":1}".into())).is_empty());
    }

    #[test]
    fn required_unset_blocks() {
        let structure = Structure {
            version: 1,
            groups: vec![GroupDef {
                name: "redis".into(),
                items: vec![str_def("host", true)],
            }],
        };
        let empty = BTreeMap::new();
        let errs = validate_publish(&empty, &structure);
        assert_eq!(errs, vec!["redis/host: required but unset".to_string()]);
    }

    #[test]
    fn required_met_passes() {
        let structure = Structure {
            version: 1,
            groups: vec![GroupDef {
                name: "redis".into(),
                items: vec![str_def("host", true)],
            }],
        };
        let mut draft = BTreeMap::new();
        draft.insert(
            "redis".into(),
            [(
                "host".into(),
                DraftValue {
                    value: Value::String("127.0.0.1".into()),
                    updated_at: 1,
                },
            )]
            .into(),
        );
        assert!(validate_publish(&draft, &structure).is_empty());
    }

    #[test]
    fn duplicate_keys_rejected() {
        let structure = Structure {
            version: 1,
            groups: vec![GroupDef {
                name: "g".into(),
                items: vec![str_def("a", false), str_def("a", false)],
            }],
        };
        assert!(!validate_structure(&structure).is_empty());
    }

    #[test]
    fn secret_flag_requires_secret_type() {
        let structure = Structure {
            version: 1,
            groups: vec![GroupDef {
                name: "g".into(),
                items: vec![ItemDef {
                    key: "s".into(),
                    ty: ValueType::String,
                    required: false,
                    secret: true,
                    validate: None,
                }],
            }],
        };
        assert!(!validate_structure(&structure).is_empty());
    }
}
