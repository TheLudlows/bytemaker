//! Minimal JSON Schema validator backing agent({schema}) + deterministic filler.
//!
//! Supports object/array/string/boolean/number/integer + required + enum +
//! properties + items. Mirrors s16's SimpleJsonSchema.

use serde_json::{json, Value};

pub struct SimpleJsonSchema {
    schema: Value,
}

impl SimpleJsonSchema {
    pub fn new(schema: Value) -> Self {
        Self { schema }
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        self.validate_value(value, &self.schema)
    }

    fn validate_value(&self, value: &Value, schema: &Value) -> Result<(), String> {
        if let Some(enum_vals) = schema.get("enum") {
            let ok = enum_vals
                .as_array()
                .map(|a| a.iter().any(|e| e == value))
                .unwrap_or(false);
            if !ok {
                return Err(format!("expected one of {enum_vals}"));
            }
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("object") => {
                let obj = value
                    .as_object()
                    .ok_or_else(|| "expected object".to_string())?;
                for key in schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let k = key.as_str().ok_or("required must be strings")?;
                    if !obj.contains_key(k) {
                        return Err(format!("missing required key '{k}'"));
                    }
                }
                if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                    for (k, sub) in props {
                        if let Some(v) = obj.get(k) {
                            self.validate_value(v, sub).map_err(|e| format!("{k}: {e}"))?;
                        }
                    }
                }
                Ok(())
            }
            Some("array") => {
                let arr = value
                    .as_array()
                    .ok_or_else(|| "expected array".to_string())?;
                if let Some(items) = schema.get("items") {
                    for (i, el) in arr.iter().enumerate() {
                        self.validate_value(el, items)
                            .map_err(|e| format!("[{i}]: {e}"))?;
                    }
                }
                Ok(())
            }
            Some("string") => {
                if value.is_string() {
                    Ok(())
                } else {
                    Err("expected string".into())
                }
            }
            Some("boolean") => {
                if value.is_boolean() {
                    Ok(())
                } else {
                    Err("expected boolean".into())
                }
            }
            Some(t) if t == "number" || t == "integer" => {
                let ok = value.is_number()
                    && !value.is_boolean()
                    && (t == "number" || value.as_i64().map(|_| true).unwrap_or(false));
                if ok {
                    Ok(())
                } else {
                    Err("expected number".into())
                }
            }
            _ => Ok(()), // no type → accept
        }
    }
}

/// Deterministic filler used by MockRunner for schemas it doesn't special-case.
pub fn fill_schema(schema: &Value, seed: &str) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let props = schema.get("properties").and_then(Value::as_object);
            let required = schema.get("required").and_then(Value::as_array);
            let keys: Vec<String> = required
                .map(|r| r.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_else(|| props.map(|p| p.keys().cloned().collect()).unwrap_or_default());
            let mut out = serde_json::Map::new();
            for k in keys {
                let sub = props.and_then(|p| p.get(&k)).cloned().unwrap_or_else(|| json!({}));
                out.insert(k.clone(), fill_schema(&sub, &format!("{seed}/{k}")));
            }
            Value::Object(out)
        }
        Some("array") => {
            let items = schema.get("items").cloned().unwrap_or_else(|| json!({}));
            Value::Array(vec![fill_schema(&items, &format!("{seed}/0"))])
        }
        Some("boolean") => Value::Bool(crate::workflow::ids::stable_hash(seed) % 4 != 0),
        Some(t) if t == "number" || t == "integer" => {
            Value::from(crate::workflow::ids::stable_hash(seed) % 5)
        }
        _ => Value::String(seed.rsplit('/').next().unwrap_or(seed).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_ok() {
        let s = SimpleJsonSchema::new(
            json!({"type":"object","required":["a"],"properties":{"a":{"type":"string"}}}),
        );
        assert!(s.validate(&json!({"a":"x"})).is_ok());
    }

    #[test]
    fn object_missing_required_fails() {
        let s = SimpleJsonSchema::new(json!({"type":"object","required":["a"]}));
        assert!(s.validate(&json!({})).is_err());
    }

    #[test]
    fn array_items_validated() {
        let s = SimpleJsonSchema::new(json!({"type":"array","items":{"type":"boolean"}}));
        assert!(s.validate(&json!([true,false])).is_ok());
        assert!(s.validate(&json!([true,"x"])).is_err());
    }

    #[test]
    fn enum_checked() {
        let s = SimpleJsonSchema::new(json!({"type":"string","enum":["high","medium","low"]}));
        assert!(s.validate(&json!("high")).is_ok());
        assert!(s.validate(&json!("nope")).is_err());
    }

    #[test]
    fn number_rejects_bool() {
        let s = SimpleJsonSchema::new(json!({"type":"number"}));
        assert!(s.validate(&json!(3)).is_ok());
        assert!(s.validate(&json!(true)).is_err());
    }

    #[test]
    fn fill_schema_object_array_scalar() {
        // Object fills required keys only (matches s16 `_fill_schema`).
        let s = json!({"type":"object","required":["a","b"],"properties":{
            "a":{"type":"string"},"b":{"type":"boolean"},"c":{"type":"array","items":{"type":"integer"}}}});
        let filled = fill_schema(&s, "seed");
        assert!(filled["a"].is_string());
        assert!(filled["b"].is_boolean());
        assert!(filled.get("c").is_none()); // optional, not filled
        assert_eq!(fill_schema(&s, "seed"), filled); // determinism

        // Array of integers.
        let arr = fill_schema(&json!({"type":"array","items":{"type":"integer"}}), "seed");
        assert!(arr.is_array());
        assert!(arr[0].is_number());

        // Scalar fallback.
        let sc = fill_schema(&json!({"type":"string"}), "seed/0");
        assert!(sc.is_string());
    }
}
