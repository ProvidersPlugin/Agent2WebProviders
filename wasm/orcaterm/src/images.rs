//! 固定 LightAI 图片上传；只传当前请求图片，不读取任意路径或跟随外链发送凭据。

use super::{
    account,
    credentials::{self, Credentials},
    upstream, LIGHTAI_BASE,
};
use agent2api_wasm_sdk::{self as sdk, http, images::Resource, json, Error, Result, Value};
use base64::Engine;

const MAX_IMAGE: usize = 10 * 1024 * 1024;
const MAX_TOTAL: usize = 50 * 1024 * 1024;
const MAX_COUNT: usize = 15;

enum ImageSource<'a> {
    Url(&'a str),
    Inline { mime: &'a str, data: &'a str },
}

pub struct Attachments {
    bytes: usize,
    count: usize,
}

impl Attachments {
    pub fn new() -> Self {
        Self { bytes: 0, count: 0 }
    }

    pub fn content(
        &mut self,
        value: &Value,
        credentials: &Credentials,
        supports_images: bool,
    ) -> Result<(String, Vec<Value>)> {
        if value.is_null() {
            return Ok((String::new(), Vec::new()));
        }
        if let Some(text) = value.as_str() {
            return Ok((text.to_string(), Vec::new()));
        }
        let parts = value
            .as_array()
            .ok_or_else(|| invalid("消息内容必须是文本或内容部件数组"))?;
        let mut text = Vec::new();
        let mut files = Vec::new();
        let mut message_bytes = 0;
        for part in parts {
            if let Some(raw) = part.as_str() {
                text.push(raw.to_string());
                continue;
            }
            if let Some(source) = image_source(part)? {
                if !supports_images {
                    return Err(invalid("当前 OrcaTerm 模型未声明图片输入"));
                }
                if files.len() >= 5 || self.count >= MAX_COUNT {
                    return Err(invalid(
                        "单条消息最多五张图片（开启工具模拟后多轮历史图片会合并为单条发送），单次请求最多十五张",
                    ));
                }
                let (resource, mime, size) = match source {
                    ImageSource::Url(url) => self::resource(url)?,
                    ImageSource::Inline { mime, data } => inline_resource(mime, data)?,
                };
                self.count += 1;
                self.bytes = self.bytes.saturating_add(size);
                message_bytes += size;
                if message_bytes > MAX_IMAGE || self.bytes > MAX_TOTAL {
                    let _ = resource.remove();
                    return Err(invalid("图片总量超过每条消息 10 MiB 或每次请求 50 MiB"));
                }
                let result = upload(&resource, &mime, credentials);
                let _ = resource.remove();
                files.push(result?);
            } else if matches!(
                part["type"].as_str(),
                Some("tool_use" | "tool_result" | "function_call" | "function_call_output")
            ) {
                return Err(Error::http(
                    400,
                    "OrcaTerm 网页没有原生工具调用接口，请开启网关工具模拟或移除工具内容块",
                )
                .with_code("orcaterm_tools_unsupported"));
            } else if matches!(
                part["type"].as_str(),
                Some("text" | "input_text" | "output_text")
            ) || part.get("type").is_none()
                && part
                    .as_object()
                    .is_some_and(|fields| fields.len() == 1 && fields.contains_key("text"))
            {
                text.push(
                    part["text"]
                        .as_str()
                        .ok_or_else(|| invalid("文本部件缺少 text"))?
                        .to_string(),
                );
            } else if !part.is_null() {
                return Err(invalid("OrcaTerm 只接通文本与图片输入，未忽略其他部件"));
            }
        }
        Ok((text.join("\n"), files))
    }
}

fn image_source(part: &Value) -> Result<Option<ImageSource<'_>>> {
    match part["type"].as_str() {
        Some("image_url" | "input_image") => {
            let target = part.get("image_url").unwrap_or(part);
            let url = target
                .as_str()
                .or_else(|| target["url"].as_str())
                .or_else(|| part["url"].as_str())
                .filter(|url| !url.trim().is_empty())
                .ok_or_else(|| invalid("图片部件缺少 URL"))?;
            Ok(Some(ImageSource::Url(url)))
        }
        Some("image") => {
            let source = &part["source"];
            match source["type"].as_str() {
                Some("base64") => Ok(Some(ImageSource::Inline {
                    mime: source["media_type"]
                        .as_str()
                        .ok_or_else(|| invalid("内联图片缺少 media_type"))?,
                    data: source["data"]
                        .as_str()
                        .ok_or_else(|| invalid("内联图片缺少 base64 数据"))?,
                })),
                Some("url") => Ok(Some(ImageSource::Url(
                    source["url"]
                        .as_str()
                        .filter(|url| !url.trim().is_empty())
                        .ok_or_else(|| invalid("图片部件缺少 URL"))?,
                ))),
                _ => Err(invalid("图片只支持 base64 数据或公网 HTTPS URL")),
            }
        }
        _ => Ok(None),
    }
}

fn normalized_mime(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "image/png" => Ok("image/png"),
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        "image/webp" => Ok("image/webp"),
        "image/gif" => Ok("image/gif"),
        _ => Err(invalid("图片只接受 PNG、JPEG、WebP 或 GIF 格式")),
    }
}

fn inline_resource(raw_mime: &str, encoded: &str) -> Result<(Resource, String, usize)> {
    let mime = normalized_mime(raw_mime)?;
    if encoded.len() > MAX_IMAGE.div_ceil(3) * 4 {
        return Err(invalid("单张图片超过 10 MiB"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid("图片 base64 无效"))?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE {
        return Err(invalid("图片为空或超过 10 MiB"));
    }
    let resource = Resource::create(if mime == "image/gif" {
        "application/octet-stream"
    } else {
        mime
    })?;
    if let Err(error) = resource.append(&bytes).and_then(|()| resource.seal()) {
        let _ = resource.remove();
        return Err(error);
    }
    Ok((resource, mime.to_string(), bytes.len()))
}

fn public_image_meta(info: &Value) -> Result<(String, usize)> {
    let size = info["size"]
        .as_u64()
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(|| invalid("图片大小未知"))?;
    let mime = info["mime"]
        .as_str()
        .filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/webp"))
        .ok_or_else(|| invalid("公共图片格式未接通"))?
        .to_string();
    if size == 0 || size > MAX_IMAGE {
        return Err(invalid("图片为空或超过 10 MiB"));
    }
    Ok((mime, size))
}

fn resource(url: &str) -> Result<(Resource, String, usize)> {
    if let Some(raw) = url.strip_prefix("data:") {
        let (prefix, encoded) = raw
            .split_once(',')
            .ok_or_else(|| invalid("图片 data URL 缺少数据"))?;
        let raw_mime = prefix
            .strip_suffix(";base64")
            .ok_or_else(|| invalid("图片 data URL 必须使用 base64 编码"))?;
        return inline_resource(raw_mime, encoded);
    }
    let parsed = url::Url::parse(url).map_err(|_| invalid("图片地址无效"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid("远程图片只接受无认证信息的 HTTPS 公网地址"));
    }
    let resource = Resource::fetch_public(url)?;
    let meta = resource.seal().and_then(|info| public_image_meta(&info));
    match meta {
        Ok((mime, size)) => Ok((resource, mime, size)),
        Err(error) => {
            let _ = resource.remove();
            Err(error)
        }
    }
}

fn upload(resource: &Resource, mime: &str, credentials: &Credentials) -> Result<Value> {
    account::validate_current(credentials)?;
    let id = credentials::random_uuid()?;
    let extension = match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    let filename = format!("{id}.{extension}");
    let path = format!("ai-uploads/{filename}");
    let mut headers = upstream::assistant_headers(credentials, &id, false);
    if let Some(fields) = headers.as_object_mut() {
        fields.remove("Content-Type");
    }
    let response = http::multipart(
        "POST",
        &format!("{LIGHTAI_BASE}/drive/llmSave"),
        headers,
        json!({ "filePath": path }),
        vec![json!({ "field": "file", "filename": filename,
            "content_type": mime, "resource": resource.id })],
    )?;
    let response = upstream::checked(response, "OrcaTerm 图片上传", &[&credentials.access_token])?;
    let bytes = response.bytes(256 * 1024)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::http(502, "OrcaTerm 图片上传未返回有效 JSON"))?;
    let data = &value["data"];
    if data["uploaded"] == false || data["passed"] == false {
        return Err(Error::http(
            502,
            "OrcaTerm 图片上传被拒绝，未忽略图片继续发送",
        ));
    }
    let preview = data["llmPreviewUrl"]
        .as_str()
        .filter(|url| !url.is_empty() && url.len() <= 8192)
        .ok_or_else(|| Error::http(502, "OrcaTerm 图片上传没有预览地址"))?;
    let mut uri =
        url::Url::parse(preview).map_err(|_| Error::http(502, "OrcaTerm 图片预览地址无效"))?;
    if uri.scheme() != "https"
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
    {
        return Err(Error::http(502, "OrcaTerm 图片预览地址不是合法 HTTPS 地址"));
    }
    let file_path = data["filePath"]
        .as_str()
        .filter(|path| !path.is_empty() && path.len() <= 8192)
        .unwrap_or(&path);
    let pairs = uri
        .query_pairs()
        .filter(|(key, _)| key != "filePath")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    uri.set_fragment(None);
    uri.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("filePath", file_path);
    let uri = uri.to_string();
    let metadata = json!({ "kind": "lightai_drive_image", "filePath": file_path,
        "llmPreviewUrl": preview, "uploadedAt": sdk::now() });
    account::validate_current(credentials)?;
    Ok(
        json!({ "name": filename, "mimeType": mime, "uri": uri, "details": {
        "acp": { "type": "image", "uri": uri, "mimeType": mime, "_meta": { "orcaterm": metadata } },
        "orcaterm": metadata } }),
    )
}

fn invalid(message: &str) -> Error {
    Error::http(400, message).with_code("orcaterm_content_unsupported")
}
