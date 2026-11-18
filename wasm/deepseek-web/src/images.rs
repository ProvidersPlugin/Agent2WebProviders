//! 每张图按原网页协议独立 PoW → multipart 上传 → 等待 SUCCESS；不重复提交失败上传。
//! 本次宿主资源与兼容内联 GIF 都按真实 MIME 上传完整原字节，不回灌大 base64。
//! 本地资源逐张释放，HTTP 句柄由 SDK 与宿主取消回收；没有证据的上游删除接口不猜。

use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use crate::{
    account::Session,
    errors,
    image_input::{self, ImageInput},
    network, util, BASE,
};

const UPLOAD_PATH: &str = "/api/v0/file/upload_file";
const PARSE_TIMEOUT: i64 = 60_000;

fn ready(session: &Session, file_id: &str) -> Result<()> {
    let started = util::now()?;
    let deadline = started.saturating_add(PARSE_TIMEOUT);
    loop {
        let now = util::now()?;
        if now < started || now >= deadline {
            return Err(Error::http(504, "DeepSeek 图片解析超时"));
        }
        let response = network::get(
            session,
            &format!("/api/v0/file/fetch_files?file_ids={}", util::query(file_id)),
        )?;
        let now = util::now()?;
        if now < started || now >= deadline {
            return Err(Error::http(504, "DeepSeek 图片解析超时"));
        }
        let file = response
            .pointer("/data/biz_data/files")
            .and_then(Value::as_array)
            .and_then(|files| files.iter().find(|file| file["id"] == file_id));
        match file.and_then(|file| file["status"].as_str()) {
            Some("SUCCESS") => return Ok(()),
            Some("PENDING" | "PARSING") | None => {}
            _ => {
                return Err(Error::http(
                    400,
                    "DeepSeek 图片解析失败，请检查图片格式与内容",
                ))
            }
        }
        sdk::sleep(500)?;
    }
}

pub fn upload(
    inputs: &[ImageInput<'_>],
    session: &Session,
    mode: &str,
    thinking: bool,
) -> Result<Vec<String>> {
    if inputs.len() > image_input::MAX_IMAGES {
        return Err(Error::http(400, "DeepSeek 单次最多上传 50 张图片"));
    }
    // prompt 已先于账号读取核对整批；上传前再核对同一调用资源，不以 JSON 大小预留额度。
    let mut reserved = image_input::validate_all(inputs)?;
    let mut file_ids = Vec::with_capacity(inputs.len());
    let mut total = 0usize;
    for (index, input) in inputs.iter().enumerate() {
        let current_local = image_input::local_bytes(input)?.unwrap_or(0);
        reserved = reserved
            .checked_sub(current_local)
            .ok_or_else(|| Error::http(400, "DeepSeek 当前图片资源大小已改变，未继续上传"))?;
        // 为所有后续本地图片保留已核实的字节预算，公网图不能先占用它们的空间。
        let remaining = image_input::MAX_TOTAL_BYTES
            .checked_sub(total)
            .and_then(|remaining| remaining.checked_sub(reserved))
            .ok_or_else(|| Error::http(400, "DeepSeek 图片累计超过 64 MiB，未继续上传"))?;
        let image = image_input::load(input, remaining)?;
        if current_local > 0 && image.size != current_local {
            return Err(Error::http(
                400,
                "DeepSeek 图片资源大小在上传前改变，未继续上传",
            ));
        }
        total += image.size;
        let proof = network::proof(session, UPLOAD_PATH)?;
        let mut headers = network::headers(session);
        headers["x-ds-pow-response"] = json!(proof);
        headers["x-model-type"] = json!(mode);
        headers["x-thinking-enabled"] = json!(if thinking { "1" } else { "0" });
        headers["x-file-size"] = json!(image.size.to_string());
        session.check()?;
        let response = http::multipart_timeout("POST", &format!("{BASE}{UPLOAD_PATH}"), headers, json!({}), vec![json!({
            "field": "file", "resource": image.id(), "filename": format!("image-{}.{}", index + 1, image.extension),
            "content_type": image.mime,
        })], 60_000).map_err(errors::network)?;
        let result = network::control(response, session)?;
        let file_id = result
            .pointer("/data/biz_data/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or_else(|| Error::http(502, "DeepSeek 图片上传未返回有效文件标识，未重新上传"))?
            .to_string();
        // 上传响应已经确认，轮询仅需要上游 file_id；释放本地资源避免积累 16 个句柄。
        drop(image);
        ready(session, &file_id)?;
        file_ids.push(file_id);
    }
    Ok(file_ids)
}
