//! 每日写动作在发送前落宿主账号台账；超时/取消不能生成新请求重复领取。
//! 日期只采用固定状态接口 sign_in_date，不从插件时间猜上游签到日。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{network::Client, sign};

const KIND: &str = "tabbitDailySignIn";

pub struct Attempt {
    value: Value,
}

impl Attempt {
    pub fn prepare(client: &Client, status: &Value, scene: &str) -> Result<Option<Self>> {
        let day = status["sign_in_date"]
            .as_str()
            .filter(|day| {
                let bytes = day.as_bytes();
                bytes.len() == 10
                    && bytes[4] == b'-'
                    && bytes[7] == b'-'
                    && bytes
                        .iter()
                        .enumerate()
                        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
            })
            .ok_or_else(|| Error::http(502, "Tabbit 签到日期未确认，未发送签到请求"))?;
        let binding = client.checkin_binding()?;
        let previous = sdk::host("ledger.get", json!({ "kind": KIND }))?;
        if !previous.is_null() && previous["version"] != 1 {
            return Err(Error::http(409, "Tabbit 原签到台账格式未知，未重新提交"));
        }
        if previous["date"] == day && previous["binding"] == binding {
            return Ok(None);
        }
        let attempt = Self {
            value: json!({ "version": 1, "date": day, "binding": binding,
                "scene": scene, "request_no": sign::random_uuid()?, "phase": "prepared" }),
        };
        client.current()?;
        sdk::host(
            "ledger.put",
            json!({ "kind": KIND, "value": attempt.value }),
        )?;
        client.current()?;
        Ok(Some(attempt))
    }

    pub fn request_no(&self) -> &str {
        self.value["request_no"].as_str().unwrap_or("")
    }

    pub fn finish(&mut self, client: &Client, result: &str) -> Result<()> {
        client.current()?;
        self.value["phase"] = json!(result);
        sdk::host("ledger.put", json!({ "kind": KIND, "value": self.value }))?;
        client.current()
    }
}
