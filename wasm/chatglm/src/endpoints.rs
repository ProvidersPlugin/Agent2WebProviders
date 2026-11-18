//! 沿用原生智谱清言已确认的 HTTPS 接口；不读取环境变量改域或广告未接入的短信端点。

pub const ORIGIN: &str = "https://chatglm.cn";
pub const GUEST: &str = "/chatglm/user-api/guest/access";
pub const REFRESH: &str = "/chatglm/user-api/user/refresh";
pub const USER: &str = "/chatglm/user-api/user/info";
pub const CHAT: &str = "/chatglm/mainchat-api/engine/stream";
pub const MODELS: &str = "/chatglm/agent-api/operation/detail?tag=available_models";
pub const MEMBER: &str = "/chatglm/member-api/member/member_info";
pub const ACTIVITY: &str = "/chatglm/member-api/member/score_activity_status";
pub const ACTIVITY_OPEN: &str = "/chatglm/member-api/member/score_activity_open";
pub const ACTIVITY_DRAW: &str = "/chatglm/member-api/member/score_activity_draw";
pub const DAILY: &str = "/chatglm/member-api/member/daily_login_score";
pub const RECORDS: &str = "/chatglm/member-api/member/score_record?page=1&page_size=50";
pub const SEASON_STATUS: &str = "/chatglm/member-api/member/summer_day_claim_status";
pub const SEASON_CLAIM: &str = "/chatglm/member-api/member/summer_day_give_score";
pub const TASK_CENTER: &str = "/chatglm/agent-api/task/center";
pub const TASK_CLICK: &str = "/chatglm/agent-api/task/click";
pub const TASK_COMPLETE: &str = "/chatglm/agent-api/task/complete";
