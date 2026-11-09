//! 网页插件的账号标识与凭证分开保存；每次操作独立绑定，不能跨账号共享。

use std::future::Future;

use agent2api_provider_sdk::{PluginCtx, StoreError};

tokio::task_local! {
    static CURRENT: String;
}

pub async fn scope<T, F: Future<Output = Result<T, String>>>(
    ctx: &PluginCtx,
    provider: &str,
    account: &str,
    work: F,
) -> Result<T, String> {
    let value = match ctx.store_get(provider, account).await {
        Ok(record) => record.public["userAgent"].as_str().unwrap_or("").trim().to_string(),
        Err(StoreError::NotFound) => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    CURRENT.scope(value, work).await
}

pub trait AccountRequest: Sized {
    fn with_account_user_agent(self) -> Self;
}

impl AccountRequest for reqwest::RequestBuilder {
    fn with_account_user_agent(self) -> Self {
        let value = CURRENT.try_with(Clone::clone).unwrap_or_default();
        if value.is_empty() { return self; }
        match reqwest::header::HeaderValue::from_str(&value) {
            Ok(value) => {
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(reqwest::header::USER_AGENT, value);
                self.headers(headers)
            }
            Err(_) => self.header(reqwest::header::USER_AGENT, value),
        }
    }
}
