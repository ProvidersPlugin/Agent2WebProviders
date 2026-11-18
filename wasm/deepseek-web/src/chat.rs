use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use crate::{
    account::Session, errors, images, models, network, prompt, sse::Reader, stream::Stream, BASE,
};

pub fn forward(params: &Value) -> Result<()> {
    let prompt = prompt::build(&params["body"])?;
    let requested = params["wire_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .or_else(|| params["body"]["model"].as_str())
        .unwrap_or("");
    if !models::accepts(requested) {
        return Err(Error::http(400, "DeepSeek 网页请选择 deepseek-web-flash"));
    }
    let model = if requested.is_empty() {
        models::MODEL_ID
    } else {
        requested
    };
    // 官方网页同账号只允许一轮生成；持锁到原流结束，不在并发生成中互相抢占。
    let _lock = sdk::AccountLock::acquire()?;
    let session = Session::load()?;
    let catalog = models::load_locked(&session, false)?;
    let mode = catalog
        .mode
        .as_deref()
        .ok_or_else(|| Error::http(403, "DeepSeek 当前账号没有可用的网页模型"))?;
    if !prompt.images.is_empty() && !catalog.images {
        return Err(Error::http(400, "DeepSeek 当前网页模型没有图片输入能力"));
    }
    let thinking = prompt::thinking(&params["body"]);
    let search = prompt::search(&params["body"])?;
    if thinking && !catalog.reasoning {
        return Err(Error::http(400, "DeepSeek 当前网页模型没有思考能力"));
    }
    let files = images::upload(&prompt.images, &session, mode, thinking)?;
    let created = network::post(&session, "/api/v0/chat_session/create", &json!({}))?;
    let chat_session = created
        .pointer("/data/biz_data/id")
        .or_else(|| created.pointer("/data/biz_data/chat_session/id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .ok_or_else(|| Error::http(502, "DeepSeek 没有返回有效会话标识"))?;
    let proof = network::proof(&session, "/api/v0/chat/completion")?;
    let body = json!({ "chat_session_id": chat_session, "parent_message_id": null, "model_type": mode,
        "prompt": prompt.text, "ref_file_ids": files, "thinking_enabled": thinking, "search_enabled": search,
        "action": null, "preempt": false });
    let mut headers = network::headers(&session);
    headers["Content-Type"] = json!("application/json");
    headers["Accept"] = json!("text/event-stream");
    headers["x-ds-pow-response"] = json!(proof);
    session.check()?;
    let response = http::open_timeout(
        "POST",
        &format!("{BASE}/api/v0/chat/completion"),
        headers,
        Some(&body),
        600_000,
    )
    .map_err(errors::network)?;
    if !(200..300).contains(&response.status)
        || network::mime(&response) != "text/event-stream"
        || network::challenge(&response)
    {
        network::control(response, &session)?;
        return Err(Error::http(502, "DeepSeek 对话未返回有效事件流"));
    }
    let mut reader = Reader::new(response);
    let mut output = Stream::new(model, thinking)?;
    // 宿主统一接收标准 Chat SSE，非流式客户端的聚合也由宿主完成。
    let result: Result<()> = (|| {
        while let Some(event) = reader.next()? {
            session.check()?;
            output.consume(&event, &session)?;
            if output.closed() {
                break;
            }
        }
        session.check()?;
        output.finish()
    })();
    if let Err(mut error) = result {
        error.message = session.redact(&error.message);
        if output.started {
            let _ = sdk::stream_event(&errors::event(&error));
        }
        return Err(error);
    }
    Ok(())
}
