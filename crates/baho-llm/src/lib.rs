//! LLM provider integration for baho.
//!
//! This crate configures provider clients. Agent behavior and prompts belong in
//! a higher-level orchestration crate once that behavior is designed.

use std::{env, io};

use llm_sdk::{XiaomiClient, error::LlmError, models::xiaomi::MIMO_V2_5_PRO, providers::XIAOMI};
use thiserror::Error;

/// Provider used when no future user-facing override is supplied.
pub const DEFAULT_PROVIDER: &str = XIAOMI;

/// Model used when no future user-facing override is supplied.
pub const DEFAULT_MODEL: &str = MIMO_V2_5_PRO;

/// Environment variable containing the Xiaomi API key.
pub const XIAOMI_API_KEY_ENV: &str = "XIAOMI_API_KEY";

/// Configuration errors raised before an LLM request is attempted.
#[derive(Debug, Error)]
pub enum ConfigurationError {
    #[error("{XIAOMI_API_KEY_ENV} is not set")]
    MissingApiKey,

    #[error("{XIAOMI_API_KEY_ENV} is not valid Unicode")]
    InvalidApiKey,

    #[error("{XIAOMI_API_KEY_ENV} must not be empty")]
    EmptyApiKey,

    #[error("could not load .env file")]
    Dotenv {
        #[source]
        source: dotenvy::Error,
    },

    #[error("could not configure Xiaomi client")]
    Client {
        #[source]
        source: LlmError,
    },
}

/// Constructs the default provider client without sending a request.
pub fn default_xiaomi_client(
    api_key: impl Into<String>,
) -> Result<XiaomiClient, ConfigurationError> {
    let api_key = api_key.into();
    if api_key.is_empty() {
        return Err(ConfigurationError::EmptyApiKey);
    }

    XiaomiClient::new(api_key)
        .map(|client| client.with_model(DEFAULT_MODEL))
        .map_err(|source| ConfigurationError::Client { source })
}

/// Constructs the default provider client from the process environment.
pub fn default_xiaomi_client_from_env() -> Result<XiaomiClient, ConfigurationError> {
    let api_key = match env::var(XIAOMI_API_KEY_ENV) {
        Ok(api_key) => api_key,
        Err(env::VarError::NotPresent) => return Err(ConfigurationError::MissingApiKey),
        Err(env::VarError::NotUnicode(_)) => return Err(ConfigurationError::InvalidApiKey),
    };
    default_xiaomi_client(api_key)
}

/// Loads a nearby `.env` file, when present, then constructs the default client.
///
/// Call this during single-threaded application startup. A missing `.env` is
/// allowed so deployments can provide `XIAOMI_API_KEY` through their environment.
pub fn default_xiaomi_client_from_dotenv() -> Result<XiaomiClient, ConfigurationError> {
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(dotenvy::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(ConfigurationError::Dotenv { source }),
    }
    default_xiaomi_client_from_env()
}
