//! 保留 event 名的有界 SSE 读取器；仅完整空行分隔的事件可以交给协议层。
//! 字节缓存跨分片保留 UTF-8、event 与多行 data；EOF 不补出一个虚假的完整帧。

use agent2api_wasm_sdk::{http, Error, Result};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_INPUT: usize = 64 * 1024 * 1024;
const MAX_FRAMES: usize = 65_536;
const MAX_READS: usize = 131_072;
// 主程序 http.read 已按这个大小切块；这里仍核对，不无界积攒房间快照。
const MAX_READ: usize = 64 * 1024;

pub struct Event {
    pub name: String,
    pub data: String,
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    name: String,
    data: String,
    has_data: bool,
    skip_lf: bool,
    frame_bytes: usize,
}

impl Decoder {
    fn push(&mut self, byte: u8) -> Result<Option<Event>> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        self.frame_bytes = self.frame_bytes.saturating_add(1);
        if self.frame_bytes > MAX_FRAME {
            return Err(Error::http(502, "Tabbit 单个事件超过大小上限"));
        }
        if !matches!(byte, b'\r' | b'\n') {
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        if self.line.is_empty() {
            self.frame_bytes = 0;
            self.has_data = false;
            // 空事件也计入帧预算；协议层忽略它，但不能让无限心跳绕过上限。
            return Ok(Some(Event {
                name: std::mem::take(&mut self.name),
                data: std::mem::take(&mut self.data),
            }));
        }
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| Error::http(502, "Tabbit 事件不是有效 UTF-8"))?;
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.name = value.to_string(),
                "data" => {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.has_data = true;
                }
                // id/retry 等元数据不作正文，也不触发重连或事件重放。
                _ => {}
            }
        }
        self.line.clear();
        Ok(None)
    }
}

pub struct Reader {
    response: http::Response,
    decoder: Decoder,
    pending: Vec<u8>,
    cursor: usize,
    input_bytes: usize,
    frames: usize,
    reads: usize,
    eof: bool,
}

impl Reader {
    pub fn new(response: http::Response) -> Self {
        Self {
            response,
            decoder: Decoder::default(),
            pending: Vec::new(),
            cursor: 0,
            input_bytes: 0,
            frames: 0,
            reads: 0,
            eof: false,
        }
    }

    /// 只消费本次已接收的字节，绝不调用 http.read；供终态后的有界尾部核对。
    pub fn buffered(&mut self) -> Result<Option<Event>> {
        while let Some(byte) = self.pending.get(self.cursor).copied() {
            self.cursor += 1;
            if let Some(event) = self.decoder.push(byte)? {
                self.frames = self.frames.saturating_add(1);
                if self.frames > MAX_FRAMES {
                    return Err(Error::http(502, "Tabbit 对话流超过事件数量上限"));
                }
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    pub fn buffered_bytes(&self) -> usize {
        self.pending
            .len()
            .saturating_sub(self.cursor)
            .saturating_add(self.decoder.frame_bytes)
    }

    pub fn incomplete(&self) -> bool {
        self.decoder.frame_bytes != 0
    }

    pub fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(event) = self.buffered()? {
                return Ok(Some(event));
            }
            if self.eof {
                return if self.incomplete() {
                    Err(Error::http(502, "Tabbit 对话流在事件结束前被截断"))
                } else {
                    Ok(None)
                };
            }
            self.reads = self.reads.saturating_add(1);
            if self.reads > MAX_READS {
                return Err(Error::http(502, "Tabbit 对话流超过读取次数上限"));
            }
            match self.response.read() {
                Ok(Some(bytes)) => {
                    self.input_bytes = self.input_bytes.saturating_add(bytes.len());
                    if bytes.len() > MAX_READ || self.input_bytes > MAX_INPUT {
                        return Err(Error::http(502, "Tabbit 对话流超过接收大小上限"));
                    }
                    self.pending = bytes;
                    self.cursor = 0;
                }
                Ok(None) => self.eof = true,
                Err(_) => return Err(Error::http(502, "Tabbit 对话流传输中断")),
            }
        }
    }
}
