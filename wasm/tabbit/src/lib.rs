//! 国内站短信协议沿用原生 Tabbit；国际站不编译或登记本模块。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod sms;
mod sms_http;
mod sms_state;
mod sms_ui;

use agent2api_wasm_sdk::{json, Result, Value};
use agent2api_wasm_tabbit_common::{self as common, Site};

const SITE: Site = Site {
    id: "tabbit",
    label: "Tabbit 国内版",
    origin: "https://web.tabbit.com",
    timezone: "Asia/Shanghai",
    fallback_sign_key: "3a9b1c8d5e7f4b209c2d1f4a6e7b8c9f",
    order: 2,
};

agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Result<Value> {
    let mut value = common::handle(SITE, "provider.info", Value::Null)?;
    if let Some(methods) = value["login_methods"].as_array_mut() {
        methods.push(json!({ "kind": "sms", "hint": "使用 11 位大陆手机号；需要滑块时请手动验证后继续发码" }));
    }
    Ok(value)
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()?])),
        "provider.info" => metadata(),
        "admin.action" if params["action"] == "auth.methods" => {
            let locale = params["payload"]["locale"].as_str().unwrap_or("zh-CN");
            let mut methods = common::handle(SITE, method, params.clone())?;
            sms_ui::decorate(&mut methods, locale);
            Ok(methods)
        }
        "admin.action" if params["action"] == "login.sms.send" => sms::send(&params),
        "admin.action" if params["action"] == "login.sms.verify" => sms::verify(&params),
        "login.cancel" => {
            sms_state::cancel(&params)?;
            Ok(Value::Null)
        }
        _ => common::handle(SITE, method, params),
    }
}
