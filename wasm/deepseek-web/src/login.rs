//! 沿用主程序现有 deepseek-web 独立窗口捕获 JSON，不启用回环 URL 回调。
//! login.interaction.v1 只用于宿主分配可信 flow；临时状态限 5 分钟并有 CAS 防重入。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use crate::{account, util, BASE};

const KEY: &str = "deepseek.login";
const TTL: i64 = 300_000;

struct Text {
    browser: &'static str,
    browser_note: &'static str,
    manual: &'static str,
    manual_note: &'static str,
    token: &'static str,
    cookie: &'static str,
    agent: &'static str,
    name: &'static str,
    optional: &'static str,
    credential: &'static str,
    waiting: &'static str,
    start: &'static str,
}

fn text(locale: &str) -> Text {
    match locale {
        "en" => Text {
            browser: "Sign in on the website", browser_note: "Sign in to the official DeepSeek site in the desktop app's separate embedded window. Login credentials are captured from this window only; web and mobile panels should paste a web token or credential JSON. This does not read your system browser or local client files.",
            manual: "Paste a web access token", manual_note: "After signing in to chat.deepseek.com, copy userToken.value from Application → Local Storage. You may paste the entire userToken object or credential JSON in the credential field. Cookie and browser user-agent are optional. This is not a paid API key. Expired login or human verification must be resolved on the official website; tokens are not automatically renewed.",
            token: "Web access token", cookie: "Website Cookie", agent: "Browser user-agent", name: "Account name", optional: "Optional", credential: "Credential JSON (optional)", waiting: "Waiting for sign-in on the official DeepSeek website.", start: "Finish signing in within 5 minutes. Complete verification on DeepSeek's official page; the desktop window captures its token and Cookie automatically.",
        },
        "zh-Hant" => Text {
            browser: "網頁登入", browser_note: "在桌面應用的獨立內嵌視窗登入 DeepSeek 官方網站，只讀取該視窗的登入態；網頁與手機面板請貼上網頁令牌或憑證 JSON。不讀取系統瀏覽器或本機客戶端檔案。",
            manual: "貼上網頁訪問令牌", manual_note: "登入 chat.deepseek.com 後，在 Application → Local Storage 複製 userToken.value，也可在憑證欄貼上完整 userToken 或憑證 JSON。Cookie 與瀏覽器標識可選。這裡不接受付費 API Key；登入過期或人機驗證需回官方網站處理，不會自動換證。",
            token: "網頁訪問令牌", cookie: "網頁 Cookie", agent: "瀏覽器標識", name: "備註名稱", optional: "可選", credential: "憑證 JSON（可選）", waiting: "等待在 DeepSeek 官方頁面完成登入。", start: "請在 5 分鐘內完成 DeepSeek 官方網頁登入；驗證碼在官方頁面手動完成，桌面視窗會自動讀取令牌與 Cookie。",
        },
        "ja" => Text {
            browser: "Web サイトでログイン", browser_note: "デスクトップアプリの独立した埋め込みウィンドウで DeepSeek 公式サイトにログインします。このウィンドウの認証情報だけを取得します。Web・モバイル画面では Web トークンか認証 JSON を貼り付けてください。システムブラウザーやクライアントのファイルは読み取りません。",
            manual: "Web アクセストークンを貼り付け", manual_note: "chat.deepseek.com にログイン後、Application → Local Storage の userToken.value をコピーします。認証欄には userToken 全体または認証 JSON も貼り付けられます。Cookie とブラウザー識別は任意です。有料 API キーではありません。期限切れや本人確認は公式サイトで対応し、自動更新は行いません。",
            token: "Web アクセストークン", cookie: "Web Cookie", agent: "ブラウザー識別", name: "アカウント名", optional: "任意", credential: "認証 JSON（任意）", waiting: "DeepSeek 公式ページでのログインを待っています。", start: "5 分以内に公式ページでログインしてください。確認操作は公式ページで行い、デスクトップウィンドウがトークンと Cookie を取得します。",
        },
        "ko" => Text {
            browser: "웹사이트 로그인", browser_note: "데스크톱 앱의 별도 내장 창에서 DeepSeek 공식 사이트에 로그인하세요. 이 창의 로그인 정보만 읽습니다. 웹·모바일 관리 화면에서는 웹 토큰이나 자격 증명 JSON을 붙여넣으세요. 시스템 브라우저나 로컬 클라이언트 파일은 읽지 않습니다.",
            manual: "웹 접근 토큰 붙여넣기", manual_note: "chat.deepseek.com에 로그인한 뒤 Application → Local Storage에서 userToken.value를 복사하세요. 자격 증명 칸에는 전체 userToken 또는 JSON도 넣을 수 있습니다. Cookie와 브라우저 식별 정보는 선택 사항입니다. 유료 API 키가 아닙니다. 만료나 사람 확인은 공식 사이트에서 처리해야 하며 자동 갱신하지 않습니다.",
            token: "웹 접근 토큰", cookie: "웹 Cookie", agent: "브라우저 식별 정보", name: "계정 이름", optional: "선택 사항", credential: "자격 증명 JSON(선택)", waiting: "DeepSeek 공식 페이지에서 로그인을 기다리는 중입니다.", start: "5분 안에 공식 페이지에서 로그인을 완료하세요. 인증은 공식 페이지에서 직접 처리하며 데스크톱 창이 토큰과 Cookie를 가져옵니다.",
        },
        "pt-BR" => Text {
            browser: "Entrar pelo site", browser_note: "Entre no site oficial do DeepSeek na janela separada do aplicativo desktop. Apenas as credenciais dessa janela são lidas. Nos painéis web e móvel, cole um token web ou JSON de credenciais. Não lê o navegador do sistema nem arquivos do cliente local.",
            manual: "Colar token de acesso web", manual_note: "Após entrar em chat.deepseek.com, copie userToken.value em Application → Local Storage. O campo de credenciais também aceita o objeto userToken completo ou JSON de credenciais. Cookie e identificação do navegador são opcionais. Não é uma chave de API paga. Expiração ou verificação humana exige voltar ao site oficial; não há renovação automática.",
            token: "Token de acesso web", cookie: "Cookie do site", agent: "Identificação do navegador", name: "Nome da conta", optional: "Opcional", credential: "JSON de credenciais (opcional)", waiting: "Aguardando login no site oficial do DeepSeek.", start: "Conclua o login em 5 minutos. Faça a verificação na página oficial; a janela desktop captura o token e o Cookie automaticamente.",
        },
        _ => Text {
            browser: "网页登录", browser_note: "在桌面应用的独立内嵌窗口登录 DeepSeek 官网，只读取该窗口的登录态；网页和手机面板请粘贴网页令牌或凭证 JSON。不读取系统浏览器或本机客户端文件。",
            manual: "粘贴网页访问令牌", manual_note: "登录 chat.deepseek.com 后，在 Application → Local Storage 复制 userToken.value，也可在凭证栏粘贴完整 userToken 或凭证 JSON。Cookie 和浏览器标识可选。这里不接受收费 API Key；登录过期或人机验证需回官方网页处理，不会自动换证。",
            token: "网页访问令牌", cookie: "网页 Cookie", agent: "浏览器标识", name: "备注名", optional: "可选", credential: "凭证 JSON（可选）", waiting: "等待在 DeepSeek 官方页面完成登录。", start: "请在 5 分钟内完成 DeepSeek 官方网页登录；验证码在官方页面手动完成，桌面窗口会自动读取令牌与 Cookie。",
        },
    }
}

pub fn methods(locale: &str) -> Value {
    let t = text(locale);
    json!({ "methods": [
        { "methodIndex": 1, "type": "oauth", "label": t.browser, "localOnly": true,
            "desktopOnly": true, "embeddedOnly": true, "manualCallback": false,
            "instructions": t.browser_note,
            "prompts": [{ "key": "name", "type": "text", "label": t.name, "placeholder": t.optional, "optional": true }] },
        { "methodIndex": 0, "type": "api", "label": t.manual, "browserMode": "none",
            "instructions": t.manual_note,
            "prompts": [
                { "key": "accessToken", "type": "text", "label": t.token, "placeholder": "userToken.value", "optional": true, "secret": true },
                { "key": "credential", "type": "text", "label": t.credential, "optional": true, "secret": true },
                { "key": "cookie", "type": "text", "label": t.cookie, "optional": true, "secret": true },
                { "key": "userAgent", "type": "text", "label": t.agent, "optional": true }
            ] }
    ] })
}

fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && flow.len() <= 256 && !flow.contains('\0'))
        .ok_or_else(|| Error::http(503, "主程序未分配 DeepSeek 登录流程"))
}

fn conflict() -> Error {
    Error::http(409, "DeepSeek 登录流程已变化，请勿重复提交").with_code("deepseek_login_conflict")
}

fn replace(snapshot: &Snapshot, value: Value, ttl: i64) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)? {
        return Err(conflict());
    }
    Ok(())
}

fn active(params: &Value) -> Result<Snapshot> {
    flow(params)?;
    let current = state::get(Scope::Login, KEY, 1)?;
    if !current.compatible || !current.value.is_object() {
        return Err(Error::http(400, "DeepSeek 登录已过期或取消，请重新发起"));
    }
    let now = util::now()?;
    if current.value["expires_at"]
        .as_i64()
        .is_none_or(|expires| expires <= now)
    {
        replace(&current, Value::Null, 1000)?;
        return Err(Error::http(400, "DeepSeek 登录已过期，请重新发起"));
    }
    Ok(current)
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let payload = &params["payload"];
    if payload["mode"] == "external" {
        return Err(Error::http(
            400,
            "DeepSeek 自动网页登录需要桌面应用的内嵌窗口，请改用粘贴网页令牌",
        ));
    }
    let locale = payload["locale"].as_str().unwrap_or("zh-CN");
    if payload
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(1))
    {
        return Err(Error::http(400, "DeepSeek 网页登录方式索引无效"));
    }
    let inputs = payload.get("inputs").cloned().unwrap_or_else(|| json!({}));
    let fields = inputs
        .as_object()
        .ok_or_else(|| Error::http(400, "DeepSeek 登录字段必须是对象"))?;
    if fields
        .iter()
        .any(|(key, value)| key != "name" || !value.is_string())
    {
        return Err(Error::http(400, "DeepSeek 网页登录包含未申报字段"));
    }
    let name: String = fields
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(100)
        .collect();
    let current = state::get(Scope::Login, KEY, 1)?;
    if !current.compatible || !current.value.is_null() {
        return Err(conflict());
    }
    replace(
        &current,
        json!({ "phase": "waiting", "name": name, "locale": locale, "expires_at": util::now()?.saturating_add(TTL) }),
        TTL,
    )?;
    Ok(
        json!({ "flow": flow, "url": format!("{BASE}/"), "poll_interval_ms": 0,
        "instructions": text(locale).start,
        "external_browser": false, "allowed_hosts": ["chat.deepseek.com", "accounts.google.com", "appleid.apple.com"] }),
    )
}

pub fn poll(params: &Value) -> Result<Value> {
    let current = active(params)?;
    match current.value["phase"].as_str() {
        Some("waiting" | "processing") => Ok(
            json!({ "state": "pending", "hint": text(current.value["locale"].as_str().unwrap_or("zh-CN")).waiting }),
        ),
        _ => Err(Error::http(400, "DeepSeek 登录已处理或取消，请重新发起")),
    }
}

pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    let current = state::get(Scope::Login, KEY, 1)?;
    replace(&current, Value::Null, 1000)?;
    Ok(Value::Null)
}

pub fn callback(params: &Value) -> Result<Value> {
    let before = active(params)?;
    if before.value["phase"] != "waiting" {
        return Err(conflict());
    }
    let code = params["code"]
        .as_str()
        .filter(|code| code.len() <= 64 * 1024)
        .ok_or_else(|| Error::http(400, "DeepSeek 登录回调缺失或过大"))?;
    let payload: Value = serde_json::from_str(code)
        .map_err(|_| Error::http(400, "DeepSeek 登录回调不是有效的凭证对象"))?;
    let fields = payload
        .as_object()
        .ok_or_else(|| Error::http(400, "DeepSeek 登录回调必须是凭证对象"))?;
    if fields.iter().any(|(key, value)| {
        !matches!(key.as_str(), "accessToken" | "cookie" | "userAgent") || !value.is_string()
    }) {
        return Err(Error::http(400, "DeepSeek 登录回调包含无效字段"));
    }
    let attempt = util::id("capture")?;
    let mut working = before.value.clone();
    working["phase"] = json!("processing");
    working["attempt"] = json!(attempt);
    replace(&before, working, TTL)?;
    // 不把任何凭证写入流程状态；HTTP 由宿主固定为该 flow 启动时的代理。
    let result = account::normalize_fields(&payload, before.value["name"].as_str());
    let current = active(params)?;
    if current.value["phase"] != "processing" || current.value["attempt"] != attempt {
        return Err(conflict());
    }
    replace(&current, Value::Null, 1000)?;
    Ok(sdk::login::done(result?))
}
