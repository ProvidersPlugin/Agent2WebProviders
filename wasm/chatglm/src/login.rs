use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let (manual, guest, note, credential, access, refresh, device, name, guest_note) = match locale {
        "en" => ("Import login credentials", "Add a guest", "Paste chatglm_token, a Cookie header, or credential JSON. Logged-in accounts with refresh tokens can renew automatically. On Windows, use the dedicated desktop “Import local login” entry and confirm the scope for this attempt. The host reads only this user’s two ChatGLM login tokens, then the plugin validates them with the upstream service. This form imports only credentials you provide and never reads local files.", "Cookie / credential JSON", "Access token", "Refresh token", "Device UUID", "Account name", "Explicitly creates and stores a guest account. Guests can chat and renew but have no points, check-in or rewards. This does not enable automatic anonymous access."),
        "zh-Hant" => ("匯入登入憑證", "新增訪客", "貼上 chatglm_token、Cookie 或憑證 JSON。有刷新令牌的登入帳號可自動續期。Windows 本機匯入請使用桌面專用「匯入本機登入態」入口，並確認本次範圍。主程式只讀取目前使用者智譜客戶端的兩項登入令牌，再由外部服務校驗；此表單僅匯入你提供的憑證，不會讀取本機檔案。", "Cookie / 憑證 JSON", "訪問令牌", "刷新令牌", "設備 UUID", "備註名稱", "明確建立並儲存一個訪客帳號。訪客可對話及換證，沒有積分、簽到或獎勵；不等於開啟零帳號自動訪客。"),
        "ja" => ("ログイン情報を取り込む", "ゲストを追加", "chatglm_token、Cookie、または認証情報 JSON を貼り付けます。更新トークンがあるアカウントは自動更新できます。Windows でのローカル取り込みは、デスクトップ専用の「ローカルのログイン状態を取り込む」から今回の範囲を確認してください。ホストは現在のユーザーの智譜クライアントにある2つのログイントークンのみを読み取り、その後サービス側で検証します。このフォームは入力された認証情報だけを取り込み、ローカルファイルは読み取りません。", "Cookie / 認証情報 JSON", "アクセストークン", "更新トークン", "デバイス UUID", "アカウント名", "ゲストアカウントを明示的に作成し保存します。会話と認証更新は可能ですが、ポイント・チェックイン・報酬はありません。匿名自動アクセスは有効になりません。"),
        "ko" => ("로그인 정보 가져오기", "게스트 추가", "chatglm_token, Cookie 또는 자격 증명 JSON을 붙여넣으세요. 갱신 토큰이 있는 계정은 자동 갱신할 수 있습니다. Windows에서는 데스크톱 전용 '로컬 로그인 가져오기'에서 이번 읽기 범위를 확인하세요. 호스트는 현재 사용자의 즈푸 클라이언트 로그인 토큰 두 항목만 읽고, 이후 서버에서 검증합니다. 이 양식은 직접 제공한 자격 증명만 가져오며 로컬 파일을 읽지 않습니다.", "Cookie / 자격 증명 JSON", "접근 토큰", "갱신 토큰", "기기 UUID", "계정 이름", "게스트 계정을 명시적으로 만들고 저장합니다. 대화와 토큰 갱신은 가능하지만 포인트, 체크인, 보상은 없습니다. 자동 익명 사용을 켜지는 않습니다."),
        "pt-BR" => ("Importar credenciais", "Adicionar convidado", "Cole chatglm_token, Cookie ou JSON de credenciais. Contas com refresh token podem renovar automaticamente. No Windows, use a opção exclusiva do desktop “Importar login local” e confirme o escopo desta tentativa. O host lê apenas os dois tokens de login do cliente ChatGLM do usuário atual, depois o serviço valida as credenciais. Este formulário importa apenas as credenciais fornecidas por você e não lê arquivos locais.", "Cookie / JSON de credenciais", "Token de acesso", "Refresh token", "UUID do dispositivo", "Nome da conta", "Cria e salva explicitamente uma conta de convidado. Convidados podem conversar e renovar credenciais, mas não têm pontos, check-in ou recompensas. Não ativa acesso anônimo automático."),
        _ => ("导入登录凭证", "添加访客", "粘贴 chatglm_token、Cookie 或凭证 JSON。有刷新令牌的登录账号可自动续期。Windows 本机导入请使用桌面专用“导入本机登录态”入口，并确认本次范围。主程序只读取当前用户智谱客户端的两项登录令牌，再由上游校验；此表单仅导入你提供的凭证，不会读取本机文件。", "Cookie / 凭证 JSON", "访问令牌", "刷新令牌", "设备 UUID", "备注名", "明确创建并保存一个访客账号。访客可对话和换证，没有积分、签到或奖励；不等于启用零账号自动访客。"),
    };
    let limit = match locale {
        "en" => "Direct SMS/password/web sign-in is not connected in this plugin. Complete any verification on the official website, then import its credentials. Images are not connected; tool calling uses gateway text emulation, not native web tools, and can be disabled in gateway settings.",
        "zh-Hant" => "此外掛尚未接入直接簡訊、密碼或網頁登入；請在官方網站自行完成驗證後匯入憑證。圖片尚未接入；工具呼叫由閘道以文字模擬，並非網頁原生工具，可在閘道設定關閉。",
        "ja" => "このプラグインではSMS・パスワード・Webの直接ログインは未接続です。公式サイトで本人確認を完了し、認証情報を取り込んでください。画像は未接続です。ツール呼び出しはゲートウェイのテキスト模擬機能で、Web本来の機能ではなく、設定で無効にできます。",
        "ko" => "이 플러그인은 SMS·비밀번호·웹 직접 로그인을 아직 연결하지 않았습니다. 공식 사이트에서 직접 인증한 후 자격 증명을 가져오세요. 이미지는 아직 연결되지 않았으며 도구 호출은 게이트웨이의 텍스트 시뮬레이션으로, 웹의 기본 기능이 아닙니다. 게이트웨이 설정에서 끌 수 있습니다.",
        "pt-BR" => "Login direto por SMS, senha ou web ainda não está conectado neste plugin. Conclua a verificação no site oficial e importe as credenciais. Imagens não estão conectadas; ferramentas usam emulação textual do gateway, não ferramentas nativas do site, e podem ser desativadas nas configurações.",
        _ => "此插件尚未接入直接短信、密码或网页登录；请在官方网页自行完成验证后导入凭证。图片尚未接入；工具调用由网关以文本模拟，并非网页原生工具，可以在网关设置中关闭。",
    };
    json!({ "instructions": limit, "methods": [
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
