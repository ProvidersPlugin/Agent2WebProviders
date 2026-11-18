use crate::Site;
use agent2api_wasm_sdk::{json, Value};

pub fn methods(site: Site, locale: &str) -> Value {
    let (label, note, cookie, token, premium, version, name) = match locale {
        "en" => ("Paste web Cookie / JWT", "Sign in on the official site and paste its token + user_id Cookie, DevTools Copy as cURL, or JWT. The two regions use separate accounts. Credentials last about 7 days; re-import after expiry. Text only; no native tool calls, image input, SMS login or automatic renewal.", "Web Cookie", "Login JWT", "Unlock Premium (0 disables)", "Client version", "Account name"),
        "zh-Hant" => ("貼上網頁 Cookie / JWT", "登入官方網站後，貼上 token 與 user_id Cookie、DevTools 的 Copy as cURL 或 JWT。兩地帳號不通用，約 7 天後到期需重新匯入。僅支援文字，不支援原生工具呼叫、圖片、簡訊登入或自動續期。", "網頁 Cookie", "登入 JWT", "解鎖 Premium（填 0 關閉）", "用戶端版本", "備註名稱"),
        "ja" => ("Web Cookie / JWT を貼り付け", "公式サイトにログインし、token と user_id の Cookie、DevTools の Copy as cURL、または JWT を貼り付けます。地域間でアカウントは共有されません。有効期限は約7日で、期限後は再取り込みが必要です。テキストのみ対応し、ネイティブのツール呼び出し・画像・SMSログイン・自動更新には対応しません。", "Web Cookie", "ログイン JWT", "Premium を有効化（0で無効）", "クライアントバージョン", "アカウント名"),
        "ko" => ("웹 Cookie / JWT 붙여넣기", "공식 사이트에 로그인한 뒤 token 및 user_id Cookie, DevTools Copy as cURL 또는 JWT를 붙여넣으세요. 지역별 계정은 별개이며 약 7일 후 만료되면 다시 가져와야 합니다. 텍스트만 지원하며 웹사이트 기본 도구 호출, 이미지, SMS 로그인 및 자동 갱신은 지원하지 않습니다.", "웹 Cookie", "로그인 JWT", "Premium 사용 (0으로 끄기)", "클라이언트 버전", "계정 이름"),
        "pt-BR" => ("Colar Cookie / JWT web", "Entre no site oficial e cole o Cookie com token e user_id, Copy as cURL do DevTools ou JWT. As regiões usam contas separadas. A sessão dura cerca de 7 dias; importe novamente após expirar. Apenas texto, sem ferramentas nativas, imagens, login por SMS ou renovação automática.", "Cookie web", "JWT de login", "Ativar Premium (0 desativa)", "Versão do cliente", "Nome da conta"),
        _ => ("粘贴网页 Cookie / JWT", "登录官方站点后粘贴 token 与 user_id Cookie、DevTools 的 Copy as cURL 或 JWT。两地区账号不通用，约 7 天到期后重新导入。仅支持文本，不支持原生工具调用、图片输入、短信登录或自动续期。", "网页 Cookie", "登录 JWT", "解锁 Premium（填 0 关闭）", "客户端版本", "备注名"),
    };
    let tools = match locale {
        "en" => "Client tool calls use the gateway's optional simulation; they are not native website execution.",
        "zh-Hant" => "客戶端工具由網關提供可關閉的模擬，不是網頁原生執行。",
        "ja" => "クライアントツールは無効化できるゲートウェイの模擬機能であり、Webサイトによるネイティブ実行ではありません。",
        "ko" => "클라이언트 도구는 끌 수 있는 게이트웨이 모의 기능이며 웹사이트의 네이티브 실행이 아닙니다.",
        "pt-BR" => "Ferramentas do cliente usam a simulação opcional do gateway, não execução nativa do site.",
        _ => "客户端工具由网关提供可关闭的模拟，不是网页原生执行。",
    };
    let mut result = json!({ "methods": [{
        "methodIndex": 0, "type": "api", "label": label, "browserMode": "none",
        "instructions": format!("{} — {}\n{note}\n{tools}", site.label, site.origin),
        "prompts": [
            { "key": "cookie", "type": "text", "label": cookie, "placeholder": "token=…; user_id=… / Copy as cURL", "secret": true, "optional": true },
            { "key": "token", "type": "text", "label": token, "placeholder": "JWT", "secret": true, "optional": true },
            { "key": "unlockPro", "type": "text", "label": premium, "placeholder": "1 / 0", "optional": true },
            { "key": "clientVersion", "type": "text", "label": version, "placeholder": crate::CLIENT_VERSION, "optional": true },
            { "key": "name", "type": "text", "label": name, "optional": true }
        ]
    }] });
    if site.id == "tabbit-intl" {
        let (label, note, field) = match locale {
            "en" => ("Import credential JSON", "Paste one credential JSON object containing cookie or accessToken/token and optionally uid, unlockPro and clientVersion. You may also paste JSON in the Cookie or JWT field above. Credentials are verified on the international site before saving; this does not import another region's account, execute cURL or read your system browser.", "Credential JSON"),
            "zh-Hant" => ("匯入憑證 JSON", "貼上一份包含 cookie 或 accessToken/token 的憑證 JSON，可附 uid、unlockPro 與 clientVersion。也可在上方 Cookie 或 JWT 欄貼上 JSON。儲存前會向國際站核對，不匯入另一地區帳號、不執行 cURL，也不讀取系統瀏覽器。", "憑證 JSON"),
            "ja" => ("認証 JSON を取り込む", "cookie または accessToken/token を含む単一の認証 JSON を貼り付けます。uid、unlockPro、clientVersion は任意です。上の Cookie・JWT 欄も JSON に対応します。保存前に国際サイトで検証し、他地域のアカウントの取り込み、cURL の実行、システムブラウザーの読み取りは行いません。", "認証 JSON"),
            "ko" => ("자격 증명 JSON 가져오기", "cookie 또는 accessToken/token이 포함된 자격 증명 JSON 한 개를 붙여넣으세요. uid, unlockPro, clientVersion은 선택 사항이며 위 Cookie·JWT 칸도 JSON을 받습니다. 저장 전에 국제 사이트에서 확인합니다. 다른 지역 계정 가져오기, cURL 실행 또는 시스템 브라우저 읽기는 하지 않습니다.", "자격 증명 JSON"),
            "pt-BR" => ("Importar JSON de credenciais", "Cole um único objeto JSON com cookie ou accessToken/token e, opcionalmente, uid, unlockPro e clientVersion. Os campos Cookie e JWT acima também aceitam JSON. As credenciais são verificadas no site internacional antes de salvar; não importa outra região, executa cURL nem lê o navegador do sistema.", "JSON de credenciais"),
            _ => ("导入凭据 JSON", "粘贴一份含 cookie 或 accessToken/token 的凭据 JSON，可附 uid、unlockPro 和 clientVersion。也可在上方 Cookie 或 JWT 栏粘贴 JSON。保存前会向国际站核对，不导入另一地区账号、不执行 cURL，也不读取系统浏览器。", "凭据 JSON"),
        };
        if let Some(methods) = result["methods"].as_array_mut() {
            methods.push(json!({ "methodIndex": 1, "type": "api", "label": label,
                "browserMode": "none", "instructions": format!("{} — {}\n{note}", site.label, site.origin),
                "prompts": [{ "key": "credential", "type": "text", "label": field,
                    "placeholder": "JSON", "secret": true }] }));
        }
    }
    result
}
