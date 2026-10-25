#![allow(dead_code)]

use agent2api_provider_sdk::{log_error, log_info, log_warn};

pub fn log(tag: &str, message: &str) {
    log_info!("{tag} {message}");
}

pub fn verbose(tag: &str, message: &str) {
    agent2api_provider_sdk::serve::emit_log("debug", format!("{tag} {message}"));
}

pub fn warn(tag: &str, message: &str) {
    log_warn!("{tag} {message}");
}

pub fn error(tag: &str, message: &str) {
    log_error!("{tag} {message}");
}

pub fn now_ms() -> i64 {
    crate::http::now_ms()
}
