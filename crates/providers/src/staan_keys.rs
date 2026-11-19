//! Staan key management (`/v1/keys`, docs.staan.ai/docs/api-keys).
//!
//! Only a console key (made in the Staan console) may call it; keys it creates can
//! only search. Staan rate-limits each search key separately (20 req/s), so one key
//! per tenant gives each tenant its own budget. The secret is returned once, by
//! the create call only. The endpoint allows 1 request/s, so a 429 is retried
//! once after a second. An account holds at most 100 keys.

use serde::Deserialize;
use std::time::Duration;

use crate::build_client;

pub const DEFAULT_BASE: &str = "https://api.staan.ai";

#[derive(Debug, Deserialize)]
pub struct CreatedKey {
    pub id: String,
    pub key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyAdminError {
    #[error("staan refused the console key ({0})")]
    Auth(u16),
    #[error("staan key limit reached (100 keys per account)")]
    LimitReached,
    #[error("staan key API rate-limited")]
    RateLimited,
    #[error("staan key API error {status}: {code}")]
    Http { status: u16, code: String },
    #[error("staan key API unreachable: {0}")]
    Network(String),
}

pub struct StaanKeys {
    client: reqwest::Client,
    base: String,
}

impl StaanKeys {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            client: build_client(15),
            base: base.into().trim_end_matches('/').to_string(),
        }
    }

    /// `name` is what the Staan console and `/v1/usage` show for the key (1-255 chars).
    pub async fn create(&self, console_key: &str, name: &str) -> Result<CreatedKey, KeyAdminError> {
        let name: String = name.chars().take(255).collect();
        let resp = self
            .send(|| {
                self.client
                    .post(format!("{}/v1/keys", self.base))
                    .bearer_auth(console_key)
                    .json(&serde_json::json!({ "name": name }))
            })
            .await?;
        resp.json::<CreatedKey>()
            .await
            .map_err(|e| KeyAdminError::Network(e.to_string()))
    }

    /// Deleting an already-deleted key (404) counts as done.
    pub async fn delete(&self, console_key: &str, id: &str) -> Result<(), KeyAdminError> {
        match self
            .send(|| {
                self.client
                    .delete(format!("{}/v1/keys/{}", self.base, urlencode(id)))
                    .bearer_auth(console_key)
            })
            .await
        {
            Ok(_) | Err(KeyAdminError::Http { status: 404, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, KeyAdminError> {
        for attempt in 0..2 {
            let resp = build()
                .send()
                .await
                .map_err(|e| KeyAdminError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                return Ok(resp);
            }
            if status == 429 && attempt == 0 {
                tokio::time::sleep(Duration::from_millis(1100)).await;
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(classify(status, &body));
        }
        Err(KeyAdminError::RateLimited)
    }
}

fn classify(status: u16, body: &str) -> KeyAdminError {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            ["code", "error", "message"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(str::to_string))
        })
        .unwrap_or_default();
    match status {
        401 => KeyAdminError::Auth(status),
        403 if code.to_ascii_lowercase().contains("limit") => KeyAdminError::LimitReached,
        403 if code.contains("SCOPE") || code.contains("DISABLED") => KeyAdminError::Auth(status),
        429 => KeyAdminError::RateLimited,
        _ => KeyAdminError::Http {
            status,
            code: code.chars().take(120).collect(),
        },
    }
}

fn urlencode(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_documented_errors() {
        assert!(matches!(
            classify(403, r#"{"error":"API key limit reached"}"#),
            KeyAdminError::LimitReached
        ));
        assert!(matches!(
            classify(403, r#"{"code":"SCOPE_NOT_GRANTED"}"#),
            KeyAdminError::Auth(403)
        ));
        assert!(matches!(classify(401, ""), KeyAdminError::Auth(401)));
        assert!(matches!(
            classify(404, "{}"),
            KeyAdminError::Http { status: 404, .. }
        ));
        assert_eq!(urlencode("6f1c-ab/../x"), "6f1c-ab%2F%2E%2E%2Fx");
    }
}
