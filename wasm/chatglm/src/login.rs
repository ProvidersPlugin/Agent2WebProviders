use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let (manual, guest, note, credential, access, refresh, device, name, guest_note) = match locale {
        "en" => ("Import login credentials", "Add a guest", "Select or paste a ChatGLM credential file, Cookie or chatglm_token to import the account; automatic renewal is supported when a refresh token is included.", "Cookie / credential JSON", "Access token", "Refresh token", "Device UUID", "Account name", "Create a guest account for chat; guests do not have points, check-in or rewards."),
        "zh-Hant" => ("匯入登入憑證", "新增訪客", "選擇或貼上智譜清言憑據檔案、Cookie 或 chatglm_token 即可匯入帳號；包含刷新令牌時支援自動續期。", "Cookie / 憑證 JSON", "訪問令牌", "刷新令牌", "設備 UUID", "備註名稱", "建立一個訪客帳號進行對話；訪客帳號不包含積分、簽到或獎勵。"),
        "ja" => ("ログイン情報を取り込む", "ゲストを追加", "ChatGLM の認証ファイル、Cookie または chatglm_token を選択・貼り付けてアカウントを取り込めます。更新トークンが含まれていれば自動更新に対応します。", "Cookie / 認証情報 JSON", "アクセストークン", "更新トークン", "デバイス UUID", "アカウント名", "ゲストアカウントを作成して会話できます。ポイント、チェックイン、報酬はありません。"),
        "ko" => ("로그인 정보 가져오기", "게스트 추가", "ChatGLM 인증 파일, Cookie 또는 chatglm_token을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 갱신 토큰이 포함되어 있으면 자동 갱신을 지원합니다.", "Cookie / 자격 증명 JSON", "접근 토큰", "갱신 토큰", "기기 UUID", "계정 이름", "대화용 게스트 계정을 생성합니다. 게스트 계정은 포인트, 출석, 보상이 없습니다."),
        "pt-BR" => ("Importar credenciais", "Adicionar convidado", "Selecione ou cole um arquivo de credenciais, Cookie ou chatglm_token do ChatGLM para importar a conta; a renovação automática é suportada quando o refresh token está incluído.", "Cookie / JSON de credenciais", "Token de acesso", "Refresh token", "UUID do dispositivo", "Nome da conta", "Cria uma conta de convidado para conversar; convidados não possuem pontos, check-in ou recompensas."),
        _ => ("导入登录凭证", "添加访客", "选择或粘贴智谱清言凭据文件、Cookie 或 chatglm_token 即可导入账号；包含刷新令牌时支持自动续期。", "Cookie / 凭证 JSON", "访问令牌", "刷新令牌", "设备 UUID", "备注名", "创建一个访客账号进行对话；访客账号不包含积分、签到或奖励。"),
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "api", "label": manual, "browserMode": "none", "instructions": note,
            "prompts": [
                { "key": "credential", "type": "text", "label": credential, "secret": true, "optional": true },
                { "key": "accessToken", "type": "text", "label": access, "secret": true, "optional": true },
                { "key": "refreshToken", "type": "text", "label": refresh, "secret": true, "optional": true },
                { "key": "deviceId", "type": "text", "label": device, "optional": true },
                { "key": "name", "type": "text", "label": name, "optional": true }
            ] },
        { "methodIndex": 1, "type": "api", "label": guest, "browserMode": "none", "instructions": guest_note,
            "prompts": [
                { "key": "deviceId", "type": "text", "label": device, "optional": true },
                { "key": "name", "type": "text", "label": name, "optional": true }
            ] }
    ] })
}
