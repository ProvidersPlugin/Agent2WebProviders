//! 智谱清言请求构造：**把 OpenAI 的多轮 messages 拍平成单条文本**。
//!
//! ── 为什么必须拍平（2026-10 实测，这不是偷懒）──────────────────
//! `mainchat-api/engine/stream` 的 `messages` **只认最后一条 user 消息**，
//! 前面的历史与 `system` 全部被丢弃。实测对照（同一组 messages，只差拍平与否）：
//!
//! ```text
//!   原生数组 [user:记住88 / assistant:好的 / user:数字是几]
//!     → 上游答「我不知道你最喜欢的数字是多少哦」         ← 历史被丢弃
//!   拍平单条 "<|user|>…<|assistant|>…<|user|>…<|assistant|>"
//!     → 上游答「88」                                      ← 历史生效
//!
//!   原生数组 [system:只回答字母A / user:介绍你自己]
//!     → 上游答「你好！我是一个由Z.ai训练的大语言模型…」   ← system 被丢弃
//!   拍平单条（system 折成 <|sytstem|> 段）
//!     → 上游答「A」                                       ← system 生效
//! ```
//!
//! 所以「拍平」不是权宜之计，而是这一家**唯一能用**的多轮表达方式
//! （与 KukuAI 的 `build_prompt`、参考实现的 `messagesPrepare` 同一取舍）。
//!
//! ── 拍平格式（逐字复刻参考实现，包括那个拼错的标记）────────────
//! ```text
//!   <|sytstem|>      ← 注意：参考实现里 system 就是这个拼写（少个 s），
//!                      上游按这个形态训练/解析，**不要"顺手修正"**
//!   <|user|>
//!   <|assistant|>
//!   <|assistant|>    ← 末尾恒追加一个，提示模型该它说话了
//! ```
//! 但实测发现**上游其实不依赖这些标记**（把标记去掉、直接拼接也能得到正确结果）；
//! 保留它们是为了贴齐已知可用的形态，不是上游的硬要求。
//!
//! ── 标记会漏进官方客户端的聊天记录（用户实测反馈，必读）────────────
//! 拍平后的那段文本不只是"给模型看的提示词"，它**就是上游存下来的那条用户
//! 消息本身**：官方客户端会把 `<|user|>` / `<|assistant|>` 原样显示在气泡里
//! （实测截图：用户从本网关发「你好」，客户端里那条消息变成
//! `<|user|>\n你好\n<|assistant|>`）。因此：
//!
//!   · **只有一段用户输入时一律原样发**（见 [`plain_single_message`]）——
//!     单轮请求没有任何历史要表达，套标记纯粹是白脏一次对话记录。实测不带
//!     任何标记的单条消息上游照常正确回答（这本来就是它的常规形态）。
//!   · 多轮 / 带 system 时仍然必须拍平（见上：数组形态的历史与 system 都被
//!     丢弃）。那种情况下标记是**功能的代价**：要么历史与 system 失效，
//!     要么对话记录里出现标记 —— 两害相权取其轻，选后者并在这里写明。
//!
//! 另外还有几条实测否证（别再试了）：`meta_data.system_prompt`、
//! 顶层 `system_prompt` / `system`、`meta_data.system` **全都被忽略**
//! （用「无论问什么都只回答字母A」的 system 逐个试过，五个位置都答了正常
//! 自我介绍）。所以 system 不能靠换字段绕开，只能折进正文。
//!
//! ── 其它实测结论（写进实现，别照搬别家的假设）──────────────────
//!   - `content` **必须是数组**（`[{type:"text", text}]`）；用字符串直接 400
//!     `invalid param`；
//!   - 多模态块（`image_url` 等）混在数组里**不报错**，但上游是否采纳未验证 ——
//!     本实现只取 text 块，其余类型丢弃（不假装支持图片）；
//!   - `conversation_id` 可省略、`meta_data` 可省略（都实测通过），
//!     本实现恒发空串/最小 meta_data 以贴合官方形态；
//!   - `selected_model` 换值确实换模型（`glm-5.3` vs `glm-5.3-flash` 上游都认）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::json::{content_parts, string_field};

use super::endpoints;

/// 拍平历史上的 role 标记（**注意 system 的拼写**，见模块头）
const MARK_SYSTEM: &str = "<|sytstem|>";
const MARK_USER: &str = "<|user|>";
const MARK_ASSISTANT: &str = "<|assistant|>";

/// 单条消息拍平出来的最大字符数（防止把超长历史整段推给上游被拒）
const MAX_FLAT_CHARS: usize = 200_000;

/// 把一条消息的 content 摊成纯文本（只取 text 块，其余类型丢弃）。
///
/// 与 `protocol::content_text` 的差别：这里**只认 `text` 类型**，
/// 不认 `input_text` / `output_text`（那是 Responses API 的块名，
/// 本家上游没这个概念，混进来只会污染历史）。
fn message_text(content: &Value) -> String {
    match content {
        // 字符串形态：上游拒收（400 invalid param），但客户端可能这么传 ——
        // 在**入站**消化掉，不当错误往外抛（下游是 OpenAI 协议，字符串是合法的）
        Value::String(text) => text.clone(),
        other => {
            let mut out = String::new();
            for part in content_parts(other) {
                if let Some(text) = part.as_str() {
                    out.push_str(text);
                    continue;
                }
                if string_field(part, "type").eq_ignore_ascii_case("text") {
                    out.push_str(&string_field(part, "text"));
                }
            }
            out
        }
    }
}

/// 拍平一批 OpenAI messages 成一条上游消息的文本。
///
/// 空消息（内容全空白）会被跳过：上游对空段没有意义，留着只会稀释上下文。
/// 末尾恒追加 `<|assistant|>`（见模块头）。
pub fn flatten_messages(messages: &[Value]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = string_field(message, "role");
        let text = message_text(message.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        let mark = match role.as_str() {
            "system" | "developer" => MARK_SYSTEM,
            "assistant" => MARK_ASSISTANT,
            // user 与一切未知 role（tool 等）都当作用户侧输入：
            // 上游没有工具消息的概念，丢掉会让上下文断裂，当作输入最不容易出错
            _ => MARK_USER,
        };
        if out.chars().count() >= MAX_FLAT_CHARS {
            break;
        }
        out.push_str(mark);
        out.push('\n');
        out.push_str(&text);
        out.push('\n');
    }
    out.push_str(MARK_ASSISTANT);
    out.push('\n');
    out
}

/// 只有一段用户输入时给出的**原样文本**（否则 `None`，交给 [`flatten_messages`]）。
///
/// ── 判据 ────────────────────────────────────────────────────
/// 跳过空白消息之后，剩下的必须**恰好一条**，且它的 role 不是
/// `system` / `developer` / `assistant` —— 那三种在拍平里带着语义
/// （system 段与"这句话是模型说的"），省掉标记就不成立了。
///
/// ── 为什么要这么做（见模块头）────────────────────────────────
/// 拍平文本就是上游存下来的那条用户消息，标记会被官方客户端显示出来。
/// 单轮请求（面板的「测试」按钮、只发一条消息的客户端）没有任何历史要表达，
/// 原样发既不影响结果，又不脏对话记录。
///
/// 返回的是**未 trim 的原文**：那是用户自己敲的字，一个空格都不该替他改。
fn plain_single_message(messages: &[Value]) -> Option<String> {
    let mut found: Option<String> = None;
    for message in messages {
        let text = message_text(message.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        if found.is_some() {
            return None;
        }
        let role = string_field(message, "role");
        if matches!(role.as_str(), "system" | "developer" | "assistant") {
            return None;
        }
        found = Some(text);
    }
    found
}

/// 从客户端请求体里读多轮消息。
///
/// 两种形态都吃：`messages`（Chat Completions）优先；
/// 没有 `messages` 时退回 `prompt` / `input`（Responses 与简化客户端会这么发）。
fn read_messages(body: &Value) -> Vec<Value> {
    if let Some(list) = body.get("messages").and_then(Value::as_array) {
        if !list.is_empty() {
            return list.clone();
        }
    }
    // 退回 prompt（字符串 / 数组两种形态）
    if let Some(prompt) = body.get("prompt") {
        match prompt {
            Value::String(text) if !text.trim().is_empty() => {
                return vec![json!({ "role": "user", "content": text })];
            }
            Value::Array(items) => {
                let mut out: Vec<Value> = Vec::new();
                for item in items {
                    if let Some(text) = item.as_str() {
                        out.push(json!({ "role": "user", "content": text }));
                    } else if item.is_object() {
                        out.push(item.clone());
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

/// 思考档位的长度上限（与网关 `model_rules::normalize` 同口径）
const MAX_LEVEL_CHARS: usize = 32;

/// 智谱清言的三个思考档位。
///
/// ── 三档只写 `meta_data.chat_mode`（2026-11 真机实测，别再加别的字段）──
/// ```text
///   档位            meta_data.chat_mode
///   快速（不思考）    ""
///   深度             "thinking"
///   极致             "deep_thinking"
/// ```
/// 模型目录里 `reasoning_efforts[]` 的 `effort` 就是这三个值。
///
/// ── 为什么不把 `reasoning_effort` 也写进信封 ──────────────────
/// 直接实测（同一账号、同一问题，`mainchat-api/engine/stream`）：
/// ```text
///   chat_mode="thinking"             → 思考（内部模型 moe_52）
///   chat_mode="thinking" + effort="" → 思考
///   chat_mode="thinking" + effort="high" → 思考
///   chat_mode="thinking" + effort="low"  → 15,000+ 字的长篇思考（不再是「快速」）
///   chat_mode="thinking" + effort="max"  → 不思考（回落 moe_5turbo）
///   chat_mode="thinking" + effort="thinking" → 不思考（同上）
///   chat_mode="deep_thinking"        → 思考
///   chat_mode="deep_thinking" + effort="high" → 不思考（同上）
///   chat_mode=""                     → 不思考
/// ```
/// `reasoning_effort` 在这个通道不是「强度档」，而是会把请求改路由到另一个
/// 内部模型；只有「不传」在三个档位上都稳定。网页通道（`backend-api`）确实
/// 同时带两个字段，但那是另一套信封 —— 本插件不走那条，不能照搬。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tier {
    Fast,
    Thinking,
    DeepThinking,
}

impl Tier {
    /// 上游 `meta_data.chat_mode` 的取值（三档开关本体）
    pub(super) fn chat_mode(self) -> &'static str {
        match self {
            Self::Fast => "",
            Self::Thinking => "thinking",
            Self::DeepThinking => "deep_thinking",
        }
    }

    /// 客户端没指定时的默认档：**深度**。
    ///
    /// 网页版对思考模型默认落在中间档；不指定就落到「快速」会让用户看到
    /// 「网页有思考、API 没有」（issue #452 的现象）。默认开思考、强度取中档
    /// 是延迟与能力的折中；要关掉思考的客户端显式传 `off` / `none` / `low`
    /// 即可（见 [`tier_of`]）。
    pub(super) fn default_tier() -> Self {
        Self::Thinking
    }
}

/// 把一个思考等级归一成三档。
///
/// 认四类写法，覆盖链路里的所有来源：
///   - 上游三档：`""` / `thinking` / `deep_thinking`；
///   - 目录强度档：`low` / `high` / `max`（`am_effort`）；
///   - 网关通用档：`minimal` … `max`（客户端与映射绑定都可能是这套）；
///   - 界面显示名：`快速` / `深度` / `极致`（模型目录归一后的档位值）。
///
/// 表外值返回 None：调用方按「没指定」回退默认档，不把未知值塞给上游。
pub(super) fn tier_of(level: &str) -> Option<Tier> {
    let level = level.trim().to_lowercase();
    match level.as_str() {
        "" | "fast" | "off" | "none" | "minimal" | "low" | "快速" => Some(Tier::Fast),
        "medium" | "high" | "thinking" | "深度" => Some(Tier::Thinking),
        "xhigh" | "max" | "deep_thinking" | "deep" | "极致" => Some(Tier::DeepThinking),
        _ => None,
    }
}

/// 客户端显式指定的思考等级（取值链与网关 `read_client_level` 逐字一致）。
///
/// 多认了两个「开关形态」：`thinking: "disabled"` 与
/// `thinking: {type:"disabled"}` → `off`；`thinking: {type:"enabled",
/// budget_tokens}` → 按预算折一档（与网关 Anthropic 入口的换算同口径）。
/// 直接走 chat 协议的客户端会这么发，Claude 协议那侧已由网关先折成
/// `reasoning_effort`，两条路都能落到同一套三档。
pub(super) fn client_level(body: &Value) -> Option<String> {
    for key in ["reasoning_effort", "reasoningEffort", "effort", "reasoning", "thinking"] {
        let Some(value) = body.get(key) else {
            continue;
        };
        match value {
            Value::String(text) => {
                let text = text.trim();
                if key == "thinking" {
                    match text.to_lowercase().as_str() {
                        "disabled" | "none" | "off" | "false" => return Some("off".to_string()),
                        "enabled" | "adaptive" | "true" => return Some("high".to_string()),
                        _ => {}
                    }
                }
                if let Some(level) = normalize_level(text) {
                    return Some(level);
                }
            }
            // responses API 的对象形态：reasoning: { effort: "high" }
            Value::Object(object) if key == "reasoning" => {
                if let Some(Value::String(effort)) = object.get("effort") {
                    if let Some(level) = normalize_level(effort) {
                        return Some(level);
                    }
                }
            }
            // Anthropic 的 thinking 对象：开关 + 预算
            Value::Object(object) if key == "thinking" => {
                let kind = object
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_lowercase();
                match kind.as_str() {
                    "disabled" | "none" | "off" => return Some("off".to_string()),
                    "enabled" | "adaptive" => {
                        let budget = object
                            .get("budget_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        return Some(budget_level(budget).to_string());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    None
}

/// 归一化一个等级字符串：去空白 → 空串 / 超长返回 None（= 不指定）
fn normalize_level(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_LEVEL_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// 思考预算 → 强度档（与网关 `protocol::anthropic::effort_from_budget` 同口径）
fn budget_level(budget: i64) -> &'static str {
    if budget <= 0 {
        "medium"
    } else if budget <= 1024 {
        "low"
    } else if budget <= 4096 {
        "medium"
    } else if budget <= 10240 {
        "high"
    } else if budget <= 32768 {
        "xhigh"
    } else {
        "max"
    }
}

/// 一次上游对话请求的构造结果
pub struct ChatPlan {
    pub url: String,
    /// 上游请求体（已是本家信封）
    pub body: Value,
}

/// 构造 `POST /chatglm/mainchat-api/engine/stream` 的请求体。
///
/// `model` 是**上游真名**（已由编排层按映射改写）：本家上游认
/// `selected_model`（如 `glm-5.3` / `glm-5.3-flash`）。空模型走上游自己的默认
/// （`result.default` 那一组）。
pub fn build_chat_body(body: &Value, model: &str) -> Value {
    let messages = read_messages(body);
    // 单轮原样发、多轮才拍平（理由见模块头：标记会显示在官方客户端里）
    let flat = plain_single_message(&messages)
        .unwrap_or_else(|| flatten_messages(&messages));
    // ── 思考档位：客户端等级 → 智谱三档 ─────────────────────────
    // 只有 `chat_mode` 进信封。**不要**再补 `reasoning_effort`：实测它会把
    // 请求改路由到另一个内部模型（低档变超长思考、高档直接不思考），
    // 三档反而全乱 —— 详见 [`Tier`] 的实测表。
    let explicit_mode = body
        .get("chat_mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let requested_tier = client_level(body).as_deref().and_then(tier_of);
    let chat_mode = match explicit_mode {
        // 智谱私有模式（`zero` / `deep_research` 等）：原样保留，不拿默认档改写
        Some(mode) if tier_of(mode).is_none() => mode.to_string(),
        // 客户端直接写了三档之一：同时给了等级时等级优先（标准字段更接近意图）
        Some(mode) => requested_tier
            .unwrap_or_else(|| tier_of(mode).unwrap_or_else(Tier::default_tier))
            .chat_mode()
            .to_string(),
        // 没写私有模式：按客户端等级翻译；什么等级都没有 → 默认「深度」
        None => requested_tier
            .unwrap_or_else(Tier::default_tier)
            .chat_mode()
            .to_string(),
    };
    let is_networking = body
        .get("is_networking")
        .or_else(|| body.get("enable_search"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let meta = json!({
        "platform": endpoints::APP_PLATFORM,
        "chat_mode": chat_mode,
        "selected_model": model,
        "is_networking": is_networking,
    });
    json!({
        "conversation_id": "",
        "messages": [{
            "role": "user",
            "content": [{ "type": "text", "text": flat }],
        }],
        "meta_data": meta,
    })
}

/// 组装完整计划（URL + 体）
pub fn build_chat_plan(body: &Value, model: &str) -> ChatPlan {
    ChatPlan {
        url: endpoints::url(endpoints::PATH_ENGINE_STREAM),
        body: build_chat_body(body, model),
    }
}
