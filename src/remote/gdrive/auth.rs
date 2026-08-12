//! OAuth 2.0 access tokens for Drive.
//!
//! Only the refresh half lives here. Obtaining the initial refresh token needs
//! a browser consent flow, which belongs in the CLI.
//!
//! Two constraints from Google shape this:
//!
//! * Refreshes are **serialized**. Without that, N concurrent FUSE operations
//!   hitting a 401 would each mint a token, and there is a hard limit of 100
//!   refresh tokens per account per OAuth client -- exceeding it silently
//!   invalidates the oldest, breaking other machines using the same client.
//! * The access token is kept **in memory** and only the refresh token is
//!   persisted. A refresh happens roughly hourly; writing to the Secret Service
//!   that often would put a D-Bus round trip on a path reachable from a FUSE
//!   request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::RemoteError;

/// Google's token endpoint. Overridable so tests can point at a mock.
pub const DEFAULT_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Refresh this long before the token actually expires, so an in-flight request
/// does not race the expiry.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct OAuthClient {
    /// Supplied by the user. There is no shared client id: Google's restricted
    /// scopes make a published one require a CASA assessment, and rclone's
    /// shared id is being retired during 2026.
    pub client_id: String,
    pub client_secret: String,
    pub token_endpoint: String,
}

impl OAuthClient {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token_endpoint: DEFAULT_TOKEN_ENDPOINT.to_string(),
        }
    }

    pub fn with_token_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.token_endpoint = endpoint.into();
        self
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    /// Google occasionally rotates the refresh token; when it does, the new one
    /// must be persisted or the old one stops working.
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        Instant::now() + EXPIRY_MARGIN < self.expires_at
    }
}

/// Mints access tokens from a refresh token.
pub struct TokenSource {
    http: reqwest::Client,
    client: OAuthClient,
    /// Guards both the cached access token and the refresh itself, so
    /// concurrent callers produce one network request rather than N.
    state: tokio::sync::Mutex<TokenState>,
    /// Set when the refresh token was rotated, so the caller can persist it.
    rotated: std::sync::Mutex<Option<String>>,
}

struct TokenState {
    refresh_token: String,
    cached: Option<CachedToken>,
}

impl TokenSource {
    pub fn new(
        http: reqwest::Client,
        client: OAuthClient,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            http,
            client,
            state: tokio::sync::Mutex::new(TokenState {
                refresh_token: refresh_token.into(),
                cached: None,
            }),
            rotated: std::sync::Mutex::new(None),
        }
    }

    /// A valid access token, refreshing only when necessary.
    pub async fn access_token(&self) -> Result<String, RemoteError> {
        let mut state = self.state.lock().await;

        if let Some(cached) = &state.cached {
            if cached.is_valid() {
                return Ok(cached.value.clone());
            }
        }

        let token = self.refresh(&state.refresh_token).await?;

        if let Some(new_refresh) = token.refresh_token.clone() {
            if new_refresh != state.refresh_token {
                tracing::info!("the refresh token was rotated; it must be persisted");
                state.refresh_token = new_refresh.clone();
                *self.rotated.lock().unwrap() = Some(new_refresh);
            }
        }

        // Google documents an hour; treat a missing value conservatively.
        let ttl = Duration::from_secs(token.expires_in.unwrap_or(3600));
        let value = token.access_token;
        state.cached = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + ttl,
        });
        Ok(value)
    }

    /// Drop the cached token, so the next call refreshes.
    ///
    /// Called after a 401: the token may have been revoked before its nominal
    /// expiry.
    pub async fn invalidate(&self) {
        self.state.lock().await.cached = None;
    }

    /// Take a rotated refresh token, if one was issued. The caller persists it.
    pub fn take_rotated_refresh_token(&self) -> Option<String> {
        self.rotated.lock().unwrap().take()
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, RemoteError> {
        let params = [
            ("client_id", self.client.client_id.as_str()),
            ("client_secret", self.client.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];

        let response = self
            .http
            .post(&self.client.token_endpoint)
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() || e.is_connect() {
                    RemoteError::Offline(e.to_string())
                } else {
                    RemoteError::Protocol(e.to_string())
                }
            })?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            let err: TokenError = serde_json::from_str(&body).unwrap_or_default();
            // `invalid_grant` means the refresh token is dead: revoked, expired
            // after 7 days because the app is still in "Testing", unused for six
            // months, or displaced by the 100-token-per-client limit. Only
            // re-consent fixes it, so it must not look retryable.
            // The description is the only part that says *why*, and
            // "invalid_grant" has half a dozen distinct causes.
            tracing::warn!(
                "token refresh failed: {} ({})",
                err.error,
                err.error_description
            );
            return Err(match err.error.as_str() {
                "invalid_grant" => RemoteError::AuthExpired,
                "invalid_client" | "unauthorized_client" => RemoteError::Unauthorized,
                _ if status.is_server_error() => RemoteError::Server {
                    status: status.as_u16(),
                    body,
                },
                _ => RemoteError::Unauthorized,
            });
        }

        serde_json::from_str(&body)
            .map_err(|e| RemoteError::Protocol(format!("malformed token response: {e}")))
    }
}

/// Shared handle.
pub type SharedTokenSource = Arc<TokenSource>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_treated_as_expired_before_it_actually_is() {
        // Otherwise a request started just under the wire arrives expired.
        let almost = CachedToken {
            value: "t".into(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(
            !almost.is_valid(),
            "a token expiring in 30s must be refreshed"
        );

        let fresh = CachedToken {
            value: "t".into(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(fresh.is_valid());
    }

    #[test]
    fn an_expired_token_is_invalid() {
        let expired = CachedToken {
            value: "t".into(),
            expires_at: Instant::now(),
        };
        assert!(!expired.is_valid());
    }

    #[test]
    fn the_token_endpoint_is_overridable_for_tests() {
        let c = OAuthClient::new("id", "secret");
        assert_eq!(c.token_endpoint, DEFAULT_TOKEN_ENDPOINT);
        let c = c.with_token_endpoint("http://127.0.0.1:1/token");
        assert_eq!(c.token_endpoint, "http://127.0.0.1:1/token");
    }

    #[test]
    fn a_token_response_without_expires_in_still_parses() {
        let r: TokenResponse = serde_json::from_str(r#"{"access_token":"abc"}"#).unwrap();
        assert_eq!(r.access_token, "abc");
        assert_eq!(r.expires_in, None);
        assert_eq!(r.refresh_token, None);
    }

    #[test]
    fn a_rotated_refresh_token_is_visible_in_the_response() {
        let r: TokenResponse = serde_json::from_str(
            r#"{"access_token":"a","expires_in":3599,"refresh_token":"new-rt"}"#,
        )
        .unwrap();
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
    }

    #[test]
    fn token_errors_parse() {
        let e: TokenError =
            serde_json::from_str(r#"{"error":"invalid_grant","error_description":"Bad"}"#).unwrap();
        assert_eq!(e.error, "invalid_grant");
        assert_eq!(e.error_description, "Bad");
    }
}
