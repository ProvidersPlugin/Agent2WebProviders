use serde_json::{json, Value};

pub fn sources(provider: &str, report: &Value) -> Vec<Value> {
    if let Some(sources) = report.get("quotaSources").and_then(Value::as_array) {
        return sources.clone();
    }
    let remaining = report
        .get("totalLeft")
        .or_else(|| report.get("available"))
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|number| number.is_finite());
    let unlimited = report.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
    if remaining.is_none() && !unlimited {
        return Vec::new();
    }
    vec![json!({
        "id": "account",
        "label": "账号主额度",
        "provider": provider,
        "unit": report.get("unit").and_then(Value::as_str).unwrap_or(""),
        "remaining": remaining,
        "total": null,
        "used": null,
        "unlimited": unlimited,
    })]
}
