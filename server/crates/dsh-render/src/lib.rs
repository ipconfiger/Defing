//! 多格式渲染（模块 08）：物化快照 → 规范化 JSON 树 → YAML/TOML/JSON。
//! 输入为解密后的普通值（secret 已由上层解密为明文或掩码）。

use std::collections::BTreeMap;

use dsh_core::error::{Error, ErrorKind};
use dsh_core::model::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Toml,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "yaml" => Ok(Format::Yaml),
            "toml" => Ok(Format::Toml),
            "json" => Ok(Format::Json),
            other => Err(Error::validation(format!("unsupported format: {other}"))),
        }
    }
}

/// 渲染器。
pub struct Renderer;

impl Renderer {
    /// 将配置快照（group → key → Value）渲染为指定格式。
    /// 注意：secret 值由调用方预先解密；若仍有密文则输出掩码占位。
    pub fn render(
        &self,
        groups: &BTreeMap<String, BTreeMap<String, Value>>,
        format: Format,
    ) -> Result<String, Error> {
        let tree = plain_tree(groups);
        match format {
            Format::Yaml => serde_yaml::to_string(&tree)
                .map_err(|e| Error::new(ErrorKind::Validation, format!("yaml: {e}"))),
            Format::Json => serde_json::to_string_pretty(&tree)
                .map_err(|e| Error::new(ErrorKind::Validation, format!("json: {e}"))),
            Format::Toml => toml::to_string(&tree).map_err(|e| {
                Error::new(
                    ErrorKind::Validation,
                    format!("toml（键需为合法标识符）: {e}"),
                )
            }),
        }
    }
}

/// Value → serde_json 普通值（去除 type 标签）。
fn plain_value(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Json(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone())),
        Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
        Value::Secret(_) => serde_json::Value::String("***".into()),
    }
}

fn plain_tree(
    groups: &BTreeMap<String, BTreeMap<String, Value>>,
) -> BTreeMap<String, BTreeMap<String, serde_json::Value>> {
    groups
        .iter()
        .map(|(g, items)| {
            let m = items
                .iter()
                .map(|(k, v)| (k.clone(), plain_value(v)))
                .collect();
            (g.clone(), m)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::model::{Ciphertext, Value};

    fn sample() -> BTreeMap<String, BTreeMap<String, Value>> {
        let mut groups = BTreeMap::new();
        groups.insert(
            "redis".into(),
            BTreeMap::from([
                ("host".into(), Value::String("127.0.0.1".into())),
                ("port".into(), Value::Int(6379)),
                ("tls".into(), Value::Bool(true)),
            ]),
        );
        groups.insert(
            "db".into(),
            BTreeMap::from([(
                "password".into(),
                Value::Secret(Ciphertext {
                    enc: "aes-256-gcm".into(),
                    v: 1,
                    dek_v: 1,
                    nonce: "n".into(),
                    ct: "c".into(),
                    edek: "e".into(),
                    edek_nonce: "en".into(),
                }),
            )]),
        );
        groups
    }

    #[test]
    fn render_json() {
        let r = Renderer;
        let out = r.render(&sample(), Format::Json).unwrap();
        assert!(out.contains("\"redis\""));
        assert!(out.contains("127.0.0.1"));
        assert!(out.contains("***")); // secret 掩码
    }

    #[test]
    fn render_yaml() {
        let r = Renderer;
        let out = r.render(&sample(), Format::Yaml).unwrap();
        assert!(out.contains("host: 127.0.0.1"));
        assert!(out.contains("port: 6379"));
    }

    #[test]
    fn render_toml() {
        let r = Renderer;
        let out = r.render(&sample(), Format::Toml).unwrap();
        assert!(out.contains("[redis]"));
        assert!(out.contains("host = \"127.0.0.1\""));
        assert!(out.contains("port = 6379"));
    }

    #[test]
    fn json_yaml_equivalence() {
        // 简单等价性：JSON 解析与 YAML 解析结果一致（浮点/整数规范化）
        let r = Renderer;
        let j = r.render(&sample(), Format::Json).unwrap();
        let y = r.render(&sample(), Format::Yaml).unwrap();
        let jv: serde_json::Value = serde_json::from_str(&j).unwrap();
        let yv: serde_json::Value = serde_yaml::from_str(&y).unwrap();
        assert_eq!(jv, yv);
    }
}
