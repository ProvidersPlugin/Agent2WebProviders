//! Tabbit 两个站点的身份与协议常量。
//!
//! 国内站 `web.tabbit.com` 与国际站 `web.tabbit.ai` 是**两套账号体系**：
//! 登录态不通用、签名 key 不同、目录与额度各按站点计。因此它们是两个
//! provider（`tabbit` / `tabbit-intl`），不是同一个 provider 上的地区字段。

/// 国内站 provider id（落盘契约，不能改）。
pub const ID_CN: &str = "tabbit";
/// 国际站 provider id（落盘契约，不能改）。
pub const ID_INTL: &str = "tabbit-intl";

/// 上游站点。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Realm {
    /// 国内站 `web.tabbit.com`。
    Cn,
    /// 国际站 `web.tabbit.ai`。
    Intl,
}

impl Realm {
    pub fn id(self) -> &'static str {
        match self {
            Realm::Cn => ID_CN,
            Realm::Intl => ID_INTL,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Realm::Cn => "Tabbit 国内版",
            Realm::Intl => "Tabbit 国际版",
        }
    }

    pub fn base(self) -> &'static str {
        match self {
            Realm::Cn => "https://web.tabbit.com",
            Realm::Intl => "https://web.tabbit.ai",
        }
    }

    /// `/chat/sign-key` 拿不到时的兜底 key。两站不同，混用会被判签名错误。
    pub fn fallback_sign_key(self) -> &'static str {
        match self {
            Realm::Cn => "3a9b1c8d5e7f4b209c2d1f4a6e7b8c9f",
            Realm::Intl => "f8d0e6a73f8d4b1a9c3d2e1f9a4b7c6d",
        }
    }

}

/// 桌面端版本号（`x-req-ctx` 的明文）。上游按 `{version}({build})` 解析，
/// 格式不对会判 493「版本过低」。可在账号上单独覆盖（`clientVersion`）。
pub const DEFAULT_CLIENT_VERSION: &str = "1.17.16(10117016)";

/// 上游前端固定的 Chrome 集成 client_id（`x-chrome-id-consistency-request`）。
pub const CLIENT_ID: &str = "2dd8eb4c1ed9c344d173";

/// 默认模型名：请求没带 model、目录也拉不到时用它，让上游自己选。
pub const DEFAULT_MODEL: &str = "default";

/// 一次请求送上游的字符预算（实测 20000 字符成功、25000 必失败，取 16000 留余量）。
pub const PROMPT_BUDGET: usize = 16000;
/// 单条消息的字符上限。
pub const PROMPT_SINGLE: usize = 9000;
/// 系统指令的字符上限。
pub const SYSTEM_BUDGET: usize = 5000;
/// 单条消息裁剪时保留的头部字符数。
pub const PROMPT_HEAD: usize = 6000;
/// 单条消息裁剪时保留的尾部字符数。
pub const PROMPT_TAIL: usize = 2000;

pub const TRUNC_NOTE: &str = "\n…[内容过长，中间部分已省略]…\n";
pub const HISTORY_NOTE: &str = "[注意：更早的对话历史因长度限制已省略]";

/// 会话缓存 TTL（上游房间 30 分钟量级）。
pub const SESSION_TTL_MS: i64 = 30 * 60 * 1000;
/// 签名 key 缓存 TTL。
pub const SIGN_KEY_TTL_MS: i64 = 30 * 60 * 1000;
/// 模型目录缓存 TTL。
pub const CATALOG_TTL_MS: i64 = 10 * 60 * 1000;
