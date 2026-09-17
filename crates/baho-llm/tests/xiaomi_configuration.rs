use baho_llm::{DEFAULT_MODEL, DEFAULT_PROVIDER, XIAOMI_API_KEY_ENV, default_xiaomi_client};
use llm_sdk::{client::LlmClient, models, providers};

#[test]
fn xiaomi_mimo_v2_5_pro_is_the_default() {
    assert_eq!(DEFAULT_PROVIDER, providers::XIAOMI);
    assert_eq!(DEFAULT_MODEL, models::xiaomi::MIMO_V2_5_PRO);

    let client = default_xiaomi_client("test-key").expect("construct Xiaomi client");
    assert_eq!(client.provider_name(), providers::XIAOMI);
    assert_eq!(client.model_name(), models::xiaomi::MIMO_V2_5_PRO);
}

#[test]
fn api_key_environment_variable_has_a_stable_name() {
    assert_eq!(XIAOMI_API_KEY_ENV, "XIAOMI_API_KEY");
}

#[test]
fn an_empty_api_key_is_rejected_without_echoing_it() {
    let error = default_xiaomi_client("").expect_err("reject an empty key");
    assert_eq!(error.to_string(), "XIAOMI_API_KEY must not be empty");
}
