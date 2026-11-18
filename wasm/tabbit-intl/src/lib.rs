#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent2api_wasm_sdk::{Result, Value};
use agent2api_wasm_tabbit_common::Site;

// 国际站是独立账号体系，不能读取国内账号或向国内 origin 发送凭据。
const SITE: Site = Site {
    id: "tabbit-intl",
    label: "Tabbit 国际版",
    origin: "https://web.tabbit.ai",
    timezone: "UTC",
    fallback_sign_key: "f8d0e6a73f8d4b1a9c3d2e1f9a4b7c6d",
    order: 3,
};

agent2api_wasm_sdk::export_provider!(handle);

fn handle(method: &str, params: Value) -> Result<Value> {
    agent2api_wasm_tabbit_common::handle(SITE, method, params)
}
