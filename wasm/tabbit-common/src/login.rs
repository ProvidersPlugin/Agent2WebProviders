use crate::Site;
use agent2api_wasm_sdk::{json, Value};

pub fn methods(site: Site, locale: &str) -> Value {
    let (label, note, cookie, token, premium, version, name) = match locale {
        "en" => ("Paste web Cookie / JWT", "Sign in on the official Tabbit site and paste the Cookie (containing token and user_id) or JWT to add the account.", "Web Cookie", "Login JWT", "Unlock Premium (0 disables)", "Client version", "Account name"),
        "zh-Hant" => ("貼上網頁 Cookie / JWT", "登入 Tabbit 官方網站後，貼上包含 token 與 user_id 的 Cookie 或 JWT 即可新增帳號。", "網頁 Cookie", "登入 JWT", "解鎖 Premium（填 0 關閉）", "用戶端版本", "備註名稱"),
        "ja" => ("Web Cookie / JWT を貼り付け", "Tabbit 公式サイトにログインし、token と user_id を含む Cookie または JWT を貼り付けてアカウントを追加できます。", "Web Cookie", "ログイン JWT", "Premium を有効化（0で無効）", "クライアントバージョン", "アカウント名"),
        "ko" => ("웹 Cookie / JWT 붙여넣기", "Tabbit 공식 사이트에 로그인한 뒤 token 및 user_id가 포함된 Cookie 또는 JWT를 붙여넣어 계정을 추가할 수 있습니다.", "웹 Cookie", "로그인 JWT", "Premium 사용 (0으로 끄기)", "클라이언트 버전", "계정 이름"),
        "pt-BR" => ("Colar Cookie / JWT web", "Entre no site oficial do Tabbit e cole o Cookie (com token e user_id) ou JWT para adicionar a conta.", "Cookie web", "JWT de login", "Ativar Premium (0 desativa)", "Versão do cliente", "Nome da conta"),
        _ => ("粘贴网页 Cookie / JWT", "登录 Tabbit 官方站点后，粘贴包含 token 与 user_id 的 Cookie 或 JWT 即可添加账号。", "网页 Cookie", "登录 JWT", "解锁 Premium（填 0 关闭）", "客户端版本", "备注名"),
    };
    let mut result = json!({ "methods": [{
        "methodIndex": 0, "type": "api", "label": label, "browserMode": "none",
        "instructions": note,
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
            "en" => ("Import credential JSON", "Select or paste a Tabbit International credential file to import the account.", "Credential JSON"),
            "zh-Hant" => ("匯入憑證 JSON", "選擇或貼上 Tabbit 國際版憑據檔案即可匯入帳號。", "憑證 JSON"),
            "ja" => ("認証 JSON を取り込む", "Tabbit 国際版の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "認証 JSON"),
            "ko" => ("자격 증명 JSON 가져오기", "Tabbit 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "인증 JSON"),
            "pt-BR" => ("Importar JSON de credenciais", "Selecione ou cole um arquivo de credenciais do Tabbit International para importar a conta.", "JSON de credenciais"),
            _ => ("导入凭据 JSON", "选择或粘贴 Tabbit 国际版凭据文件即可导入账号。", "凭据 JSON"),
        };
        if let Some(methods) = result["methods"].as_array_mut() {
            methods.push(json!({ "methodIndex": 1, "type": "api", "label": label,
                "browserMode": "none", "instructions": note,
                "prompts": [{ "key": "credential", "type": "text", "label": field,
                    "placeholder": "JSON", "secret": true }] }));
        }
    }
    result
}
