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
        "en" => ("Phone verification code", "Mainland China phone", "Use an 11-digit mainland China number. If a Meituan verification page appears, complete it yourself, return here and send again; the same login session is retained. Cookie / JWT import remains available. Text only, without images or automatic renewal. Client tools use the gateway's optional simulation, not native website execution."),
        "zh-Hant" => ("手機驗證碼登入", "大陸手機號", "使用 11 位大陸手機號。若出現美團驗證頁，請手動完成後回來繼續發碼，本輪登入標識會保留。也可貼上 Cookie / JWT。僅支援文字，不支援圖片或自動續期；客戶端工具透過可關閉的網關模擬，不是網頁原生執行。"),
        "ja" => ("SMS コードでログイン", "中国本土の携帯番号", "中国本土の11桁の番号を使用します。美団の確認画面が出たら手動で完了し、戻って再送してください。同じログインセッションを維持します。Cookie / JWT の取り込みも可能です。テキストのみ対応し、画像・自動更新には対応しません。クライアントツールは無効化できるゲートウェイの模擬機能であり、Webサイトによるネイティブ実行ではありません。"),
        "ko" => ("SMS 코드로 로그인", "중국 본토 휴대폰 번호", "중국 본토 11자리 번호를 사용하세요. 메이투안 인증 페이지가 나타나면 직접 완료한 뒤 돌아와 다시 보내세요. 동일한 로그인 세션을 유지합니다. Cookie / JWT도 가져올 수 있습니다. 텍스트만 지원하며 이미지와 자동 갱신은 지원하지 않습니다. 클라이언트 도구는 끌 수 있는 게이트웨이 모의 기능이며 웹사이트의 네이티브 실행이 아닙니다."),
        "pt-BR" => ("Entrar com código SMS", "Telefone da China continental", "Use um número de 11 dígitos da China continental. Se surgir a verificação da Meituan, conclua-a manualmente, volte e envie novamente; a mesma sessão é mantida. Também é possível importar Cookie / JWT. Apenas texto, sem imagens ou renovação automática. Ferramentas do cliente usam a simulação opcional do gateway, não execução nativa do site."),
        _ => ("手机验证码登录", "大陆手机号", "使用 11 位大陆手机号。若出现美团验证页，请手动完成后回到这里继续发码，本轮登录标识会保留。也可粘贴 Cookie / JWT。仅支持文本，不支持图片或自动续期；客户端工具由网关提供可关闭的模拟，不是网页原生执行。"),
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
        for item in items.iter_mut() {
            item["instructions"] = json!(format!(
                "{} — {}\n{note}",
                crate::SITE.label,
                crate::SITE.origin
            ));
        }
        // methodIndex 0 仍是旧凭证导入，不改已有方法索引；展示顺序优先短信。
        items.insert(0, method);
    }
}
