#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod content_block;
#[cfg(windows)]
mod crypto;
mod errors;
mod headers;
mod http;
mod json;
mod logging;
mod onboarding;
mod providers;
mod quota;
mod store;

fn main() {
    agent2api_provider_sdk::serve_blocking(providers::registry());
}
