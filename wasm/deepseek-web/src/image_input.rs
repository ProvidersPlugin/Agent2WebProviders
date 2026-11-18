//! 宿主入场图片只借用本次资源，不把大图转回 JSON/base64 或创建第二份资源。
//! 兼容旧内联输入；GIF 保留完整原字节，不抽帧、不转码。
//! 公共图仍仅走 fetch_public 的 PNG/JPEG/WebP，不扩大普通 HTTP 权限。
//! 最多 15 张本地图片、50 张总图，单图 32 MiB/总 64 MiB；剩余 JSON 仍受 16 MiB ABI 限制。

use std::collections::BTreeSet;

use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};
use base64::Engine;

use crate::image_gif;

pub const MAX_IMAGES: usize = 50;
pub const MAX_LOCAL_IMAGES: usize = 15;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ABI_BYTES: usize = 16 * 1024 * 1024;
const BASE64_CHUNK: usize = 64 * 1024;
const RESOURCE_CHUNK: usize = 64 * 1024;

pub enum ImageInput<'a> {
    Inline {
        mime: &'static str,
        encoded: &'a str,
    },
    Inbound(Inbound<'a>),
    Public(&'a str),
}

/// 字段不公开；只有固定宿主输入块能解析出候选，候选本身不持有清理权限。
pub struct Inbound<'a> {
    id: &'a str,
    mime: &'static str,
    size: usize,
}

fn inbound(part: &Value) -> Result<ImageInput<'_>> {
    let fields = part
        .as_object()
        .filter(|fields| {
            fields.len() == 4
                && ["type", "resource", "mime", "size"]
                    .iter()
                    .all(|key| fields.contains_key(*key))
        })
        .ok_or_else(|| invalid("DeepSeek 宿主图片块格式无效，未接收额外资源材料"))?;
    let id = fields["resource"]
        .as_str()
        .filter(|id| {
            id.len() == 37
                && id.starts_with("blob_")
                && id.as_bytes()[5..]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
        .ok_or_else(|| invalid("DeepSeek 宿主图片资源标识格式无效"))?;
    let actual_mime = fields["mime"]
        .as_str()
        .filter(|value| {
            matches!(
                *value,
                "image/png" | "image/jpeg" | "image/webp" | "image/gif"
            )
        })
        .ok_or_else(|| invalid("DeepSeek 宿主图片类型无效，不猜测实际格式"))?;
    let size = fields["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= MAX_IMAGE_BYTES as u64)
        .ok_or_else(|| invalid("DeepSeek 宿主图片大小无效或超过单图 32 MiB"))?
        as usize;
    Ok(ImageInput::Inbound(Inbound {
        id,
        mime: mime(actual_mime)?,
        size,
    }))
}

fn invalid(message: &str) -> Error {
    Error::http(400, message)
}

fn mime(value: &str) -> Result<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "image/png" => Ok("image/png"),
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        "image/webp" => Ok("image/webp"),
        "image/gif" => Ok("image/gif"),
        _ => Err(invalid(
            "DeepSeek 内联图片仅支持 PNG、JPEG、WebP 或 GIF；不会把其他格式改名上传",
        )),
    }
}

fn inline<'a>(mime_type: &str, encoded: &'a str) -> Result<ImageInput<'a>> {
    if encoded.is_empty() {
        return Err(invalid("DeepSeek 图片内容不能为空"));
    }
    if encoded.len() >= MAX_ABI_BYTES {
        return Err(invalid(
            "DeepSeek 兼容内联图片仍计入 16 MiB 调用消息；大图必须由支持入场资源化的主程序转换，不能直接塞回 base64",
        ));
    }
    Ok(ImageInput::Inline {
        mime: mime(mime_type)?,
        encoded,
    })
}

fn source(value: &str) -> Result<ImageInput<'_>> {
    if let Some(value) = value.strip_prefix("data:") {
        let (metadata, encoded) = value
            .split_once(',')
            .ok_or_else(|| invalid("DeepSeek 图片 data URL 无效"))?;
        let mut fields = metadata.split(';');
        let mime_type = fields.next().unwrap_or("");
        if fields.next() != Some("base64") || fields.next().is_some() {
            return Err(invalid(
                "DeepSeek 内联图片须使用 data:image/格式;base64,内容，不能包含其他编码参数",
            ));
        }
        return inline(mime_type, encoded);
    }
    if value.len() > 8192 {
        return Err(invalid("DeepSeek 图片 URL 超过长度限制"));
    }
    if value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        return Err(invalid(
            "旧版公网 HTTP 图片链接在 WASM 中不再允许；请提供原站 HTTPS 443 图片链接或符合入站限制的 base64 图片",
        ));
    }
    if !value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        return Err(invalid(
            "DeepSeek WASM 公共图片仅支持 HTTPS 443 地址，不接受本机路径或其他协议",
        ));
    }
    // 这里只做纯语法预检；逐跳 DNS/IP 与端口许可仍由原 fetch_public 裁决。
    // 不自动把 HTTP 改 HTTPS、不做 URL 规范化，也不看扩展名猜格式。
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
        || value.contains(['\\', '#'])
    {
        return Err(invalid(
            "DeepSeek 公共图片地址不能包含空白、控制字符、反斜杠或片段",
        ));
    }
    let authority = value[8..].split(['/', '?']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(invalid("DeepSeek 公共图片地址不能包含登录凭据或空主机"));
    }
    let port = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .filter(|end| *end > 1)
            .ok_or_else(|| invalid("DeepSeek 公共图片地址格式无效"))?;
        let suffix = &authority[end + 1..];
        if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| invalid("DeepSeek 公共图片地址格式无效"))?,
            )
        }
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    if port.is_some_and(|port| {
        port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().ok() != Some(443)
    }) {
        return Err(invalid("DeepSeek 公共图片仅支持 HTTPS 443 端口"));
    }
    Ok(ImageInput::Public(value))
}

pub fn input(part: &Value) -> Result<Option<ImageInput<'_>>> {
    match part["type"].as_str() {
        Some("image_url" | "input_image") => {
            let value = part.get("image_url").unwrap_or(part);
            let url = value
                .as_str()
                .or_else(|| value["url"].as_str())
                .filter(|url| !url.trim().is_empty())
                .ok_or_else(|| invalid("DeepSeek 图片缺少 data URL 或公共 HTTPS URL"))?;
            Ok(Some(source(url)?))
        }
        Some("image") => {
            let source = &part["source"];
            match source["type"].as_str() {
                Some("base64") => Ok(Some(inline(
                    source["media_type"].as_str().ok_or_else(|| {
                        invalid("DeepSeek 内联图片缺少明确的 media_type，不会默认当作 PNG")
                    })?,
                    source["data"]
                        .as_str()
                        .ok_or_else(|| invalid("DeepSeek 图片缺少 base64 内容"))?,
                )?)),
                Some("url") => {
                    let url = source["url"]
                        .as_str()
                        .ok_or_else(|| invalid("DeepSeek 图片缺少 URL"))?;
                    Ok(Some(self::source(url)?))
                }
                _ => Err(invalid("DeepSeek 图片只接受 base64 内容或公共 HTTPS URL")),
            }
        }
        Some("image_resource") => Ok(Some(inbound(part)?)),
        _ => Ok(None),
    }
}

pub struct Temporary(Resource);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = sdk::host("resource.remove", json!({ "id": self.0.id }));
    }
}

pub struct Image {
    resource: Temporary,
    pub mime: &'static str,
    pub extension: &'static str,
    pub size: usize,
}

impl Image {
    pub fn id(&self) -> &str {
        &self.resource.0.id
    }
}

fn format(bytes: &[u8]) -> Result<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(("image/png", "png"));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Ok(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Ok(("image/webp", "webp"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok(("image/gif", "gif"));
    }
    Err(invalid(
        "DeepSeek 图片内容不是 PNG、JPEG、WebP 或 GIF，不会把其他文件当图片上传",
    ))
}

fn resource_mime(actual: &str) -> &str {
    // resources.v1 明确支持通用二进制，multipart 从封存资源取原字节且独立声明真实 MIME。
    // 这不是 image/png 伪装，不调用图片输出 materialize，也不绕过公共图片下载的格式限制。
    if actual == "image/gif" {
        "application/octet-stream"
    } else {
        actual
    }
}

fn chunks(encoded: &str, mut consume: impl FnMut(usize, &[u8]) -> Result<()>) -> Result<usize> {
    let mut size = 0usize;
    for (index, chunk) in encoded.as_bytes().chunks(BASE64_CHUNK).enumerate() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk)
            .map_err(|_| invalid("DeepSeek 图片 base64 无效"))?;
        // 非末片不能出现 padding，避免接受多个独立 base64 串的拼接。
        if index * BASE64_CHUNK + chunk.len() < encoded.len() && chunk.contains(&b'=') {
            return Err(invalid("DeepSeek 图片 base64 中间出现结束填充"));
        }
        size = size.saturating_add(bytes.len());
        if size > MAX_IMAGE_BYTES {
            return Err(invalid("DeepSeek 图片超过单图 32 MiB 资源上限"));
        }
        consume(index, &bytes)?;
    }
    if size == 0 {
        return Err(invalid("DeepSeek 图片内容不能为空"));
    }
    Ok(size)
}

fn validate_inline(mime: &str, encoded: &str) -> Result<usize> {
    let mut gif = (mime == "image/gif").then(image_gif::Validator::new);
    let size = chunks(encoded, |index, bytes| {
        if index == 0 && format(bytes)?.0 != mime {
            return Err(invalid("DeepSeek 图片声明类型与实际编码不一致"));
        }
        if let Some(gif) = &mut gif {
            gif.push(bytes)?;
        }
        Ok(())
    })?;
    if let Some(gif) = gif {
        gif.finish()?;
    }
    Ok(size)
}

/// 只读候选，不创建 Temporary；无效 ID/类型/大小绝不触发 resource.remove。
fn inspect_inbound(input: &Inbound<'_>) -> Result<(&'static str, &'static str, usize)> {
    let metadata = sdk::host("resource.seal", json!({ "id": input.id }))?;
    if metadata["id"].as_str() != Some(input.id) {
        return Err(invalid("DeepSeek 图片元数据不属于原宿主资源"));
    }
    let size = metadata["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= MAX_IMAGE_BYTES as u64)
        .ok_or_else(|| invalid("DeepSeek 宿主图片实际大小无效"))? as usize;
    if size != input.size || metadata["mime"].as_str() != Some(resource_mime(input.mime)) {
        return Err(invalid(
            "DeepSeek 宿主图片声明与本次资源的真实大小或存储类型不一致",
        ));
    }
    // 不通过 SDK 的宽松 read 兜底猜空片；回包大小、完成状态与分片长度也必须真实匹配。
    let first = sdk::host(
        "resource.read",
        json!({ "id": input.id, "offset": 0, "length": RESOURCE_CHUNK }),
    )?;
    let data = first["data"]
        .as_str()
        .filter(|data| data.len() <= RESOURCE_CHUNK.div_ceil(3) * 4)
        .ok_or_else(|| invalid("DeepSeek 宿主图片首片格式无效"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid("DeepSeek 宿主图片首片编码无效"))?;
    if first["size"].as_u64() != Some(size as u64)
        || first["done"].as_bool() != Some(size <= RESOURCE_CHUNK)
        || bytes.len() != size.min(RESOURCE_CHUNK)
    {
        return Err(invalid("DeepSeek 宿主图片首片未保留原始字节数"));
    }
    let (actual, extension) = format(&bytes)?;
    if actual != input.mime {
        return Err(invalid("DeepSeek 宿主图片真实魔数与声明格式不一致"));
    }
    // GIF 的完整块在主程序入场时已校验；这里只取真实首片，不抽帧或假装像素解码。
    Ok((actual, extension, size))
}

/// 本地已知输入的真实字节数；公网 URL 返回 None，不在此下载。
pub fn local_bytes(input: &ImageInput<'_>) -> Result<Option<usize>> {
    match input {
        ImageInput::Inline { mime, encoded } => validate_inline(mime, encoded).map(Some),
        ImageInput::Inbound(input) => inspect_inbound(input).map(|(_, _, size)| Some(size)),
        ImageInput::Public(_) => Ok(None),
    }
}

/// 必须先于插件账号/网络请求；纯内联解析和本次宿主资源只读核对，不预取 URL。
pub fn validate_all(inputs: &[ImageInput<'_>]) -> Result<usize> {
    if inputs.len() > MAX_IMAGES {
        return Err(invalid("DeepSeek 单次最多上传 50 张图片"));
    }
    let mut total = 0usize;
    let mut encoded_total = 0usize;
    let mut local_count = 0usize;
    let mut ids = BTreeSet::new();
    // 第一遍先核对整批可计算限制和重复标识，避免部分核对/上传后才发现已知问题。
    for input in inputs {
        match input {
            ImageInput::Inline { encoded, .. } => {
                local_count += 1;
                encoded_total = encoded_total.saturating_add(encoded.len());
            }
            ImageInput::Inbound(input) => {
                local_count += 1;
                if !ids.insert(input.id) {
                    return Err(invalid(
                        "DeepSeek 同次图片重复引用宿主资源，未再次读取或上传",
                    ));
                }
                total = total.saturating_add(input.size);
            }
            ImageInput::Public(_) => {}
        }
    }
    if local_count > MAX_LOCAL_IMAGES {
        return Err(invalid(
            "DeepSeek 本次最多 15 张内联或宿主资源图片；保留原 16 个资源槽中的一个",
        ));
    }
    if encoded_total >= MAX_ABI_BYTES {
        return Err(invalid(
            "DeepSeek 兼容内联图片仍计入 16 MiB 调用消息；大图必须由支持入场资源化的主程序转换，不能直接塞回 base64",
        ));
    }
    if total > MAX_TOTAL_BYTES {
        return Err(invalid("DeepSeek 宿主图片累计超过 64 MiB 资源上限"));
    }
    total = 0;
    // 第二遍才查询当前调用资源。只有全部通过，才能进入后续账号与上传流程。
    for input in inputs {
        if let Some(size) = local_bytes(input)? {
            total = total.saturating_add(size);
            if total > MAX_TOTAL_BYTES {
                return Err(invalid("DeepSeek 本地图片累计超过 64 MiB 资源上限"));
            }
        }
    }
    Ok(total)
}

fn inspect(resource: Temporary, remaining: usize, allow_inline_gif: bool) -> Result<Image> {
    let metadata = resource.0.seal()?;
    let size = metadata["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= MAX_IMAGE_BYTES as u64 && *size <= remaining as u64)
        .ok_or_else(|| invalid("DeepSeek 图片超过单图 32 MiB 或累计 64 MiB 资源上限"))?
        as usize;
    // 只读首个 64 KiB 分片识别编码；整张图留在宿主，以资源句柄交给 multipart。
    let first = resource.0.read(0)?;
    let (actual, extension) = format(&first)?;
    if actual == "image/gif" && !allow_inline_gif {
        return Err(invalid(
            "DeepSeek WASM 公网 GIF 下载尚未接入；GIF 请使用符合入站限制的 base64 内容",
        ));
    }
    if metadata["mime"].as_str() != Some(resource_mime(actual)) {
        return Err(invalid("DeepSeek 图片资源类型与实际编码不一致"));
    }
    Ok(Image {
        resource,
        mime: actual,
        extension,
        size,
    })
}

pub fn load(input: &ImageInput<'_>, remaining: usize) -> Result<Image> {
    if remaining == 0 {
        return Err(invalid("DeepSeek 单次图片总量不能超过 64 MiB 资源上限"));
    }
    match input {
        ImageInput::Inbound(input) => {
            // 候选没有析构；所有真实核对先完成，错误 ID 不传给 remove。
            let (actual, extension, size) = inspect_inbound(input)?;
            if size > remaining {
                return Err(invalid("DeepSeek 图片超过累计 64 MiB 资源上限"));
            }
            // 仅此时借用已确认属于当前调用的原资源，不复制大图或再创建 blob。
            // 上传成功/失败结束后 Image 的析构回收该图，其他尚未上传资源由宿主 Context 持有。
            Ok(Image {
                resource: Temporary(Resource {
                    id: input.id.to_string(),
                }),
                mime: actual,
                extension,
                size,
            })
        }
        ImageInput::Public(url) => {
            // 宿主执行 DNS/IP/跳转检查，不带账号凭据、不走账号或系统代理。
            // 宿主不接受公网 GIF；禁止改走普通 http.open 或通过后缀/响应头猜格式。
            inspect(Temporary(Resource::fetch_public(url)?), remaining, false)
        }
        ImageInput::Inline { mime, encoded } => {
            // 整批已在 prompt 中预检；这里再次从不可变借用核对，不复用外部传入的资源 ID。
            let expected = validate_inline(mime, encoded)?;
            if expected > remaining {
                return Err(invalid("DeepSeek 图片超过累计 64 MiB 资源上限"));
            }
            let resource = Temporary(Resource::create(resource_mime(mime))?);
            chunks(encoded, |_, bytes| resource.0.append(bytes))?;
            let image = inspect(resource, remaining, true)?;
            if image.size != expected || image.mime != *mime {
                return Err(invalid(
                    "DeepSeek 图片资源未完整保留原始编码或字节数，未上传",
                ));
            }
            Ok(image)
        }
    }
}
