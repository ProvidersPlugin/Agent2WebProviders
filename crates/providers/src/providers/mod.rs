#[cfg(feature = "chatglm")]
pub mod chatglm;

use agent2api_provider_sdk::BoxProvider;

pub fn registry() -> Vec<BoxProvider> {
    let mut list: Vec<BoxProvider> = Vec::new();
    #[cfg(feature = "chatglm")]
    list.push(Box::new(chatglm::ChatglmProvider));
    list
}
