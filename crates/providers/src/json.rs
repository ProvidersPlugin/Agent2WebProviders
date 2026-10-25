use serde_json::Value;

pub fn string_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn string_field(value: &Value, key: &str) -> String {
    value.get(key).map(string_value).unwrap_or_default()
}

pub fn content_parts(content: &Value) -> &[Value] {
    match content.as_array() {
        Some(parts) => parts.as_slice(),
        None => &[],
    }
}
