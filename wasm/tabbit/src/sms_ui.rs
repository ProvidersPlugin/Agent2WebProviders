use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Config},
    Value,
};

pub const METHOD: u64 = 1;
// 11 位手机号、6 位验证码和 60 秒重发沿用旧 ui/sms-login.js。
// 5 分钟是本地登录流程安全期限（主程序 LOGIN_TIMEOUT_MS），不是猜测短信上游有效期。
pub const RESEND_SECONDS: u64 = 60;
pub const EXPIRES_SECONDS: u64 = 300;

pub fn decorate(methods: &mut Value, locale: &str) {
    let (label, phone, note) = match locale {
        "en" => ("Phone verification code", "Mainland China phone", "Enter an 11-digit mainland China phone number and the SMS verification code to sign in."),
        "zh-Hant" => ("手機驗證碼登入", "大陸手機號", "輸入 11 位大陸手機號與簡訊驗證碼即可登入。"),
        "ja" => ("SMS コードでログイン", "中国本土の携帯番号", "中国本土の11桁の携帯番号と SMS 認証コードを入力してログインします。"),
        "ko" => ("SMS 코드로 로그인", "중국 본토 휴대폰 번호", "중국 본토 11자리 휴대폰 번호와 SMS 인증 코드를 입력해 로그인합니다."),
        "pt-BR" => ("Entrar com código SMS", "Telefone da China continental", "Informe um número de 11 dígitos da China continental e o código SMS para entrar."),
        _ => ("手机验证码登录", "大陆手机号", "输入 11 位大陆手机号与短信验证码即可登录。"),
    };
    let mut method = sms::method(
        METHOD,
        label,
        Config {
            phone_label: phone.into(),
            phone_placeholder: "1xxxxxxxxxx".into(),
            country_codes: Vec::new(),
            phone_min_length: 11,
            phone_max_length: 11,
            code_min_length: 6,
            code_max_length: 6,
            resend_after_seconds: RESEND_SECONDS,
            expires_after_seconds: EXPIRES_SECONDS,
        },
    );
    method["instructions"] = json!(note);
    if let Some(items) = methods["methods"].as_array_mut() {
        // methodIndex 0 仍是旧凭证导入，不改已有方法索引；展示顺序优先短信。
        items.insert(0, method);
    }
}
