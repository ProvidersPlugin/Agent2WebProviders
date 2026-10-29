//! DeepSeek 网页登录账号，使用正常会话创建与工作量证明；遇到人机验证要求重新登录。
//! 网页与收费 API 账号凭证不是同一用途，不把 API 模型能力当作网页能力。
//! 依据 kittors/deepseek-web-api (MIT, 2026-10-24) 的请求与 SSE patch 字段。
//! 尚未发现网页模型目录、独立余额、签到、领取或 refresh 接口：参考客户端只使用
//! default/expert 两个网页模式，不伪称在线模型刷新或自动续期。

mod pow;
mod prompt;
mod stream;

use std::time::Duration;
use md5::{Digest, Md5};
use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{control_error, ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use futures::StreamExt;
use serde_json::{json, Value};

pub const ID: &str = "deepseek-web";
const BASE: &str = "https://chat.deepseek.com";

pub struct DeepseekWebProvider;

pub fn id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() { return format!("{prefix}-{}", crate::http::now_ms()); }
    format!("{prefix}-{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

fn models() -> Vec<Value> {
    ["deepseek-web-flash", "deepseek-web-pro"].into_iter().map(|id| json!({ "id": id, "name": id,
        "supportsImages": false, "supportsToolCall": true, "supportsReasoning": true,
        "toolCallMode": "emulated", "synthetic": true })).collect()
}

fn client(proxy: Option<&proto::WireProxy>, gateway: &str) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(15));
    let url = proxy.map(proto::WireProxy::to_url).unwrap_or_else(|| gateway.to_string());
    if !url.trim().is_empty() && url != "direct" { builder = builder.proxy(reqwest::Proxy::all(&url).map_err(|_| "DeepSeek 出网代理无效")?); }
    builder.build().map_err(|error| format!("DeepSeek 网络客户端创建失败：{error}"))
}

fn headers(token: &str, cookie: &str) -> Vec<(String, String)> {
    let mut headers = vec![("Authorization".into(), format!("Bearer {token}")), ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json".into()), ("Origin".into(), BASE.into()), ("Referer".into(), format!("{BASE}/")),
        ("x-client-platform".into(), "web".into()), ("x-client-version".into(), "2.2.0".into()), ("x-client-locale".into(), "zh_CN".into()),
        ("x-client-bundle-id".into(), "com.deepseek.chat".into())];
    if !cookie.is_empty() { headers.push(("Cookie".into(), cookie.into())); }
    headers
}

async fn post(client: &reqwest::Client, path: &str, headers: &[(String, String)], body: Value) -> Result<Value, String> {
    let mut request = client.post(format!("{BASE}{path}")).json(&body).timeout(Duration::from_secs(30));
    for (name, value) in headers { request = request.header(name, value); }
    let response = request.send().await.map_err(|error| format!("DeepSeek 请求失败：{error}"))?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|_| "DeepSeek 响应读取失败")?;
    if bytes.len() > 1024 * 1024 { return Err("DeepSeek 控制响应过大".into()); }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| control_error(403, "DeepSeek 网页要求人机验证，请在官方网页手动登录后更新 Cookie", None, None))?;
    if !(200..300).contains(&status) || value["code"].as_i64().is_some_and(|code| code != 0) || value.pointer("/data/biz_code").and_then(Value::as_i64).is_some_and(|code| code != 0) {
        let message = value.get("msg").or_else(|| value.pointer("/data/biz_msg")).and_then(Value::as_str).unwrap_or("DeepSeek 网页请求失败");
        return Err(control_error(if status >= 400 { i32::from(status) } else { 400 }, message, None, None));
    }
    Ok(value)
}

async fn prepare_and_forward(ctx: &PluginCtx, req: proto::ConversationRequest, sink: &mut ConversationSink<'_>, head_sent: &mut bool) -> Result<(), String> {
    let record = ctx.store_get(ID, &req.account).await.map_err(|_| "DeepSeek 网页账号不存在")?;
    let token = record.secret["accessToken"].as_str().filter(|token| !token.is_empty()).ok_or("DeepSeek 网页账号缺少访问令牌")?;
    let cookie = record.secret["cookie"].as_str().unwrap_or("");
    let headers = headers(token, cookie);
    let client = client(req.proxy.as_ref(), &ctx.env.proxy)?;
    let model = if req.wire_model.is_empty() { req.body["model"].as_str().unwrap_or("") } else { &req.wire_model };
    let model_type = match model { "deepseek-web-flash" => "default", "deepseek-web-pro" => "expert", _ => return Err("DeepSeek 网页请选择 deepseek-web-flash 或 deepseek-web-pro".into()) };
    let prompt = prompt::build(&req.body)?;
    let created = post(&client, "/api/v0/chat_session/create", &headers, json!({})).await?;
    let session = created.pointer("/data/biz_data/id").or_else(|| created.pointer("/data/biz_data/chat_session/id")).and_then(Value::as_str).ok_or("DeepSeek 没有返回会话 id")?;
    let challenge = post(&client, "/api/v0/chat/create_pow_challenge", &headers, json!({ "target_path": "/api/v0/chat/completion" })).await?;
    let challenge = challenge.pointer("/data/biz_data/challenge").cloned().ok_or("DeepSeek 没有返回验证 challenge")?;
    let proof = tokio::task::spawn_blocking(move || pow::solve(&challenge)).await.map_err(|_| "DeepSeek 验证任务失败")??;
    let thinking = !matches!(req.body["reasoning_effort"].as_str(), Some("none" | "off"));
    let mut request = client.post(format!("{BASE}/api/v0/chat/completion")).json(&json!({
        "chat_session_id": session, "parent_message_id": null, "model_type": model_type, "prompt": prompt,
        "ref_file_ids": [], "thinking_enabled": thinking, "search_enabled": false, "action": null, "preempt": false,
    })).header("x-ds-pow-response", proof).timeout(Duration::from_secs(600));
    for (name, value) in &headers { request = request.header(name, value); }
    let response = request.header("Accept", "text/event-stream").send().await.map_err(|error| format!("DeepSeek 对话发送失败：{error}"))?;
    if !response.status().is_success() { return Err(control_error(i32::from(response.status().as_u16()), "DeepSeek 对话被上游拒绝，请检查账号与 Cookie", None, None)); }
    if !response.headers().get("content-type").and_then(|value| value.to_str().ok()).is_some_and(|mime| mime.contains("text/event-stream")) {
        return Err(control_error(403, "DeepSeek 网页要求重新登录或人机验证", None, None));
    }
    let response_id = id("chatcmpl");
    let tool_mode = req.body.get("tools").and_then(Value::as_array).is_some_and(|tools| !tools.is_empty());
    let streaming = req.stream && !tool_mode;
    let mut decoder = stream::Decoder::default();
    let mut utf8 = Vec::new();
    let mut source = response.bytes_stream();
    sink.head(proto::StreamHead { status: 200, headers: [("content-type".into(), "text/event-stream".into())].into_iter().collect(), upstream: proto::UpstreamKind::Chat }).await?;
    *head_sent = true;
    while let Some(bytes) = source.next().await {
        let bytes = bytes.map_err(|_| "DeepSeek 对话传输中断")?;
        let before_content = decoder.content.len();
        let before_reasoning = decoder.reasoning.len();
        utf8.extend_from_slice(&bytes);
        let valid = match std::str::from_utf8(&utf8) { Ok(text) => text.len(), Err(error) if error.error_len().is_none() => error.valid_up_to(), Err(_) => return Err("DeepSeek 响应不是 UTF-8".into()) };
        let text = std::str::from_utf8(&utf8[..valid]).map_err(|_| "DeepSeek 响应无法解码")?.replace("\r\n", "\n");
        decoder.feed(&text)?; utf8.drain(..valid);
        if decoder.content.len() + decoder.reasoning.len() > 8 * 1024 * 1024 { return Err("DeepSeek 响应超过大小上限".into()); }
        if streaming {
            let mut delta = json!({});
            if decoder.content.len() > before_content { delta["content"] = json!(&decoder.content[before_content..]); }
            if decoder.reasoning.len() > before_reasoning { delta["reasoning_content"] = json!(&decoder.reasoning[before_reasoning..]); }
            if delta.as_object().is_some_and(|value| !value.is_empty()) { send_delta(sink, &response_id, model, delta, None).await?; }
        }
    }
    if !utf8.is_empty() { return Err("DeepSeek 响应被截断".into()); }
    decoder.finish()?;
    if streaming {
        send_delta(sink, &response_id, model, json!({}), Some("stop")).await?;
        if decoder.total_tokens > 0 { sink.send(format!("data: {}\n\n", json!({ "id": response_id, "object": "chat.completion.chunk", "model": model, "choices": [], "usage": { "total_tokens": decoder.total_tokens } })).as_bytes()).await?; }
        sink.send(b"data: [DONE]\n\n").await?;
    } else {
        let calls = prompt::parse_calls(&decoder.content, &req.body)?;
        let body = stream::completion(&response_id, model, &decoder, calls);
        send_result_stream(sink, &body).await?;
    }
    Ok(())
}

async fn send_result_stream(sink: &mut ConversationSink<'_>, body: &Value) -> Result<(), String> {
    let message = &body["choices"][0]["message"];
    let mut delta = message.clone();
    if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) { for (index, call) in calls.iter_mut().enumerate() { call["index"] = json!(index); } }
    let finish = body["choices"][0]["finish_reason"].as_str();
    send_delta(sink, body["id"].as_str().unwrap_or(""), body["model"].as_str().unwrap_or(""), delta, None).await?;
    send_delta(sink, body["id"].as_str().unwrap_or(""), body["model"].as_str().unwrap_or(""), json!({}), finish).await?;
    if let Some(usage) = body.get("usage") { sink.send(format!("data: {}\n\n", json!({ "id": body["id"], "object": "chat.completion.chunk", "model": body["model"], "choices": [], "usage": usage })).as_bytes()).await?; }
    sink.send(b"data: [DONE]\n\n").await
}

async fn send_delta(sink: &mut ConversationSink<'_>, id: &str, model: &str, delta: Value, finish: Option<&str>) -> Result<(), String> {
    sink.send(format!("data: {}\n\n", json!({ "id": id, "object": "chat.completion.chunk", "created": crate::http::now_ms() / 1000, "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })).as_bytes()).await
}

impl Provider for DeepseekWebProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "DeepSeek 网页").with_capabilities(proto::Capabilities { chat: true, stateful: true, ..Default::default() })
            .with_models(models()).with_login(vec![proto::LoginMethod::Token { label: "网页访问令牌".into(), placeholder: "chat.deepseek.com 的 userToken.value，不是收费 API Key".into(),
                extra_fields: vec![proto::LoginField { key: "cookie".into(), label: "网页 Cookie".into(), placeholder: "官方网页正常登录后的 Cookie；风控时请手动更新".into(), optional: true, multiline: true }] }])
            .with_account_features(proto::AccountFeatures { identifier: vec!["uid".into()], ..Default::default() })
    }
    fn conversation<'a>(&'a self, ctx: &'a PluginCtx, req: proto::ConversationRequest, sink: ConversationSink<'a>) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            let mut head_sent = false;
            match prepare_and_forward(ctx, req, &mut sink, &mut head_sent).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    if head_sent { return Err(error); }
                    let failure = error.strip_prefix("CONTROL_ERROR:").and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                    if let Some(failure) = failure {
                        let status = failure["status"].as_u64().filter(|status| (400..600).contains(status)).unwrap_or(502) as u16;
                        sink.head(proto::StreamHead { status, headers: [("content-type".into(), "application/json".into())].into_iter().collect(), upstream: proto::UpstreamKind::Chat }).await?;
                        sink.send(json!({ "message": failure["message"] }).to_string().as_bytes()).await?;
                        Ok(())
                    } else { Err(error) }
                }
            }
        })
    }
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { ctx.store_get(ID, account).await.map_err(|error| error.to_string())?.secret["accessToken"].as_str().map(str::to_string).ok_or_else(|| "DeepSeek 网页缺少令牌".into()) })
    }
    fn models<'a>(&'a self, _ctx: &'a PluginCtx, _account: &'a str) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(proto::ModelList { models: models(), meta: Default::default() }) })
    }
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body["message"].as_str().unwrap_or("DeepSeek 网页请求失败").to_string();
        match status { 401 => proto::ErrorClass::TokenExpired { message }, 429 => proto::ErrorClass::QuotaLimited { reset_at: None, message, upstream_code: None, status },
            _ => proto::ErrorClass::Fatal { status, message, upstream_code: None } }
    }
    fn normalize_account<'a>(&'a self, ctx: &'a PluginCtx, input: proto::NormalizeInput) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let payload = match input.source { proto::NormalizeSource::Manual { payload } => payload, proto::NormalizeSource::FileText { content, .. } => serde_json::from_str(&content).unwrap_or_else(|_| json!({ "token": content })), _ => return Err("DeepSeek 网页请粘贴本人登录令牌与 Cookie".into()) };
            let token = ["accessToken", "token", "apiKey"].into_iter().find_map(|key| payload[key].as_str()).unwrap_or("").trim().trim_start_matches("Bearer ").to_string();
            let cookie = payload["cookie"].as_str().unwrap_or("").trim().to_string();
            if token.is_empty() || token.len() > 16384 || token.starts_with("sk-") || cookie.len() > 32768 || token.contains(['\r', '\n']) || cookie.contains(['\r', '\n']) { return Err("请填写有效的 DeepSeek 网页访问令牌，不是收费 API Key".into()); }
            let client = client(None, &ctx.env.proxy)?;
            let headers = headers(&token, &cookie);
            let response = post(&client, "/api/v0/chat/create_pow_challenge", &headers, json!({ "target_path": "/api/v0/chat/completion" })).await?;
            if response.pointer("/data/biz_data/challenge").is_none() { return Err("DeepSeek 登录态校验失败：未返回验证 challenge".into()); }
            let tail: String = token.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
            let uid = format!("{:x}", Md5::digest(token.as_bytes()));
            Ok(proto::NormalizedAccount { identity: format!("{ID}-{uid}"), secret: json!({ "accessToken": token, "cookie": cookie }),
                public: json!({ "name": input.name.unwrap_or_else(|| format!("DeepSeek {tail}")), "tokenTail": tail, "uid": uid, "credentialIdentity": true }), desktop: false })
        })
    }
}
