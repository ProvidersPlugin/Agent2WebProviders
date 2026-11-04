#[cfg(feature = "chatglm")]
pub mod chatglm;
#[cfg(feature = "deepseek")]
pub mod deepseek;
#[cfg(feature = "tabbit")]
pub mod tabbit;

use agent2api_provider_sdk::BoxProvider;

pub fn registry() -> Vec<BoxProvider> {
    let mut list: Vec<BoxProvider> = Vec::new();
    #[cfg(feature = "chatglm")]
    list.push(Box::new(chatglm::ChatglmProvider));
    #[cfg(feature = "deepseek")]
    list.push(Box::new(deepseek::DeepseekWebProvider));
    #[cfg(feature = "tabbit")]
    {
        // 国内版在前、国际版紧随其后（与 AutoClaw / Qoder 的成对排列一致）。
        list.push(Box::new(tabbit::TabbitProvider::new(
            tabbit::config::Realm::Cn,
        )));
        list.push(Box::new(tabbit::TabbitProvider::new(
            tabbit::config::Realm::Intl,
        )));
    }
    list
}
