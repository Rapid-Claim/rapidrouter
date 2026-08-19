//! Signing a Codex seat back in from the console.
//!
//! A seat whose refresh token has been revoked is unrecoverable by the
//! refresher — the only fix is a fresh login, and until now that meant an
//! operator with a shell on the box running the Codex CLI and copying an
//! `auth.json` into place. A pool of eighty seats makes that a full
//! afternoon, which is why eighteen of them sat expired.
//!
//! The browser flow the CLI runs by default cannot be used from here: it
//! pins its redirect to `http://localhost:1455`, which is the operator's
//! laptop, not the gateway. Device-code login is the same client id
//! reaching the same tokens without any redirect back to us. The operator
//! signs in wherever they have a browser; this process polls until the
//! code is claimed and then writes the credential itself.
//!
//! Three properties this inherits from [`crate::refresh`], for the same
//! reasons:
//!
//! - **Persist before publish.** The document is written to disk and
//!   re-parsed before it becomes the live credential, so a credential we
//!   could not record never serves traffic.
//! - **A failed login changes nothing.** The seat keeps whatever it had.
//! - **Nowhere to write means no login.** A seat configured inline or from
//!   an environment variable has no file to own; signing it in would
//!   succeed upstream and be forgotten on restart.
//!
//! What it does *not* inherit is the breaker: a completed login clears no
//! bench. A seat benched for a spent weekly window is still spent — the
//! quota belongs to the account, not the token — and un-benching it here
//! would send traffic straight back into a 429.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use http::StatusCode;
use http_body_util::BodyExt;
use router_core::config::ProviderKind;
use router_core::credential;
use router_core::vkey;
use router_core::{ErrorClass, GatewayError};
use router_providers::subscription;
use serde_json::Value;

use crate::AppState;
use crate::upstream::UpstreamClient;

/// How long a finished login is remembered, so the console can read the
/// outcome it was waiting for before the record disappears.
const RETAIN_FINISHED_MS: u64 = 5 * 60 * 1000;

/// Where a login has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The code is minted and outstanding.
    Waiting,
    /// Tokens exchanged, written, and live on the seat.
    Signed { email: Option<String> },
    /// Nothing changed; the seat kept what it had.
    Failed { reason: String },
}

impl Outcome {
    pub fn as_json(&self) -> Value {
        match self {
            Outcome::Waiting => serde_json::json!({ "state": "waiting" }),
            Outcome::Signed { email } => serde_json::json!({ "state": "signed", "email": email }),
            Outcome::Failed { reason } => {
                serde_json::json!({ "state": "failed", "reason": reason })
            }
        }
    }
}

/// One login, from the moment a code is minted.
#[derive(Debug, Clone)]
pub struct DeviceLogin {
    pub provider: String,
    pub key: String,
    pub user_code: String,
    pub verification_url: String,
    /// When the one-time code stops being accepted.
    pub expires_at_ms: u64,
    pub outcome: Outcome,
    finished_at_ms: Option<u64>,
}

/// Logins in flight, keyed by an id the console holds.
#[derive(Default)]
pub struct DeviceLoginRegistry {
    sessions: Mutex<HashMap<String, DeviceLogin>>,
}

impl DeviceLoginRegistry {
    pub fn get(&self, id: &str) -> Option<DeviceLogin> {
        self.sessions
            .lock()
            .expect("login registry")
            .get(id)
            .cloned()
    }

    fn insert(&self, id: String, login: DeviceLogin) {
        let mut sessions = self.sessions.lock().expect("login registry");
        sessions.insert(id, login);
    }

    fn settle(&self, id: &str, outcome: Outcome, now_ms: u64) {
        let mut sessions = self.sessions.lock().expect("login registry");
        if let Some(login) = sessions.get_mut(id) {
            login.outcome = outcome;
            login.finished_at_ms = Some(now_ms);
        }
    }

    /// Drop records nobody can still be waiting on: finished and read, or
    /// outstanding past the point the code would be accepted anyway.
    fn reap(&self, now_ms: u64) {
        let mut sessions = self.sessions.lock().expect("login registry");
        sessions.retain(|_, login| match login.finished_at_ms {
            Some(at) => now_ms.saturating_sub(at) < RETAIN_FINISHED_MS,
            None => now_ms < login.expires_at_ms.saturating_add(RETAIN_FINISHED_MS),
        });
    }

    /// Whether this seat already has a login outstanding.
    ///
    /// Two codes for one seat is not dangerous, only confusing: whichever
    /// the operator ignores keeps polling, and the console has no way to
    /// say which of the two codes on screen is the live one.
    fn outstanding(&self, provider: &str, key: &str, now_ms: u64) -> Option<(String, DeviceLogin)> {
        let sessions = self.sessions.lock().expect("login registry");
        sessions
            .iter()
            .find(|(_, login)| {
                login.provider == provider
                    && login.key == key
                    && login.outcome == Outcome::Waiting
                    && now_ms < login.expires_at_ms
            })
            .map(|(id, login)| (id.clone(), login.clone()))
    }
}

/// Why a login could not even be started.
pub struct Refusal {
    pub status: StatusCode,
    pub message: String,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        message: message.into(),
    }
}

/// Mint a one-time code for a seat and start polling for it.
///
/// Returns the id the console polls, along with what to put on screen.
pub async fn start(
    state: &Arc<AppState>,
    provider_name: &str,
    key_name: &str,
) -> Result<(String, DeviceLogin), Refusal> {
    let now_ms = vkey::unix_now_ms();
    state.logins.reap(now_ms);

    let table = state.table.load();
    let Some(provider) = table.providers().find(|p| p.name == provider_name) else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("no provider `{provider_name}`"),
        ));
    };
    // Only Codex: this is Codex's own device endpoint and its own client
    // id. A Claude seat is renewed by its CLI, and pointing this at one
    // would mint a code that can never be claimed.
    if provider.kind != ProviderKind::CodexSubscription {
        return Err(refuse(
            StatusCode::CONFLICT,
            "device login is a Codex flow; this provider signs in another way",
        ));
    }
    let Some(key) = provider.keys.iter().find(|k| k.name == key_name) else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("no credential `{key_name}`"),
        ));
    };
    if key.credential.seat().is_none() {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this credential is not a subscription seat",
        ));
    }
    if key.source_path.is_none() {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this seat is configured inline or from the environment, so a new \
             credential would have nowhere to be written — point it at a \
             credential file first",
        ));
    }

    // An outstanding code is handed back rather than replaced: the
    // operator may simply have closed the dialog, and minting a second
    // code would leave two on screen with no way to tell which is live.
    if let Some((id, existing)) = state.logins.outstanding(provider_name, key_name, now_ms) {
        return Ok((id, existing));
    }

    let minted = request_user_code(&state.upstream).await.map_err(|err| {
        refuse(
            StatusCode::BAD_GATEWAY,
            format!("could not get a login code from OpenAI: {err}"),
        )
    })?;

    let login = DeviceLogin {
        provider: provider_name.to_owned(),
        key: key_name.to_owned(),
        user_code: minted.user_code.clone(),
        verification_url: subscription::CODEX_DEVICE_VERIFICATION_URL.to_owned(),
        expires_at_ms: now_ms + subscription::CODEX_DEVICE_CODE_TTL_S * 1000,
        outcome: Outcome::Waiting,
        finished_at_ms: None,
    };
    let id = format!("dl_{}", uuid::Uuid::now_v7().simple());
    state.logins.insert(id.clone(), login.clone());

    // Polled here rather than from the console: the exchange rotates a
    // refresh token that must be written to disk, and a browser tab that
    // gets closed halfway through would otherwise strand a live
    // credential nobody recorded.
    tokio::spawn(poll_until_claimed(
        Arc::clone(state),
        id.clone(),
        minted,
        provider_name.to_owned(),
        key_name.to_owned(),
    ));

    Ok((id, login))
}

/// What the usercode endpoint hands back.
#[derive(Debug, Clone)]
struct Minted {
    device_auth_id: String,
    user_code: String,
    interval_s: u64,
}

async fn request_user_code(client: &UpstreamClient) -> Result<Minted, GatewayError> {
    let body = post_json(
        client,
        subscription::CODEX_DEVICE_USERCODE_URL,
        subscription::codex_device_usercode_body(),
    )
    .await?;
    if !body.status.is_success() {
        return Err(GatewayError::new(
            ErrorClass::UpstreamError,
            format!("device code request refused with {}", body.status),
        ));
    }
    let value: Value = serde_json::from_slice(&body.bytes).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("device code response was not JSON: {e}"),
        )
    })?;
    let string = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let (Some(device_auth_id), Some(user_code)) = (
        string("device_auth_id"),
        string("user_code").or_else(|| string("usercode")),
    ) else {
        return Err(GatewayError::new(
            ErrorClass::UpstreamError,
            "device code response carried no code",
        ));
    };
    Ok(Minted {
        device_auth_id,
        user_code,
        // Sent as a string by this endpoint, so a number is accepted too
        // rather than assumed.
        interval_s: value
            .get("interval")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .filter(|i| *i > 0)
            .unwrap_or(subscription::CODEX_DEVICE_POLL_INTERVAL_S),
    })
}

/// Poll until the operator claims the code, then install what it buys.
async fn poll_until_claimed(
    state: Arc<AppState>,
    id: String,
    minted: Minted,
    provider_name: String,
    key_name: String,
) {
    let deadline_ms = vkey::unix_now_ms() + subscription::CODEX_DEVICE_CODE_TTL_S * 1000;
    let interval = std::time::Duration::from_secs(minted.interval_s);

    let outcome = loop {
        if vkey::unix_now_ms() >= deadline_ms {
            break Outcome::Failed {
                reason: "the code expired before it was entered".into(),
            };
        }
        tokio::time::sleep(interval).await;

        let response = match post_json(
            &state.upstream,
            subscription::CODEX_DEVICE_TOKEN_URL,
            subscription::codex_device_poll_body(&minted.device_auth_id, &minted.user_code),
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                // A transport blip is not a refusal; the code is good for
                // fifteen minutes and the next poll may well land.
                tracing::debug!(error = %err, "device login poll failed; retrying");
                continue;
            }
        };

        if response.status.is_success() {
            break claim(&state, &provider_name, &key_name, &response.bytes).await;
        }
        // Outstanding, not refused: this endpoint says "not yet" with a
        // 403 or a 404 rather than the `authorization_pending` an RFC 8628
        // client would expect.
        if response.status == StatusCode::FORBIDDEN || response.status == StatusCode::NOT_FOUND {
            continue;
        }
        break Outcome::Failed {
            reason: format!("OpenAI refused the login with {}", response.status),
        };
    };

    if let Outcome::Failed { reason } = &outcome {
        tracing::warn!(provider = %provider_name, key = %key_name, reason, "device login did not complete");
    }
    state.logins.settle(&id, outcome, vkey::unix_now_ms());
}

/// Exchange the claimed code and install the credential it buys.
async fn claim(
    state: &Arc<AppState>,
    provider_name: &str,
    key_name: &str,
    poll_body: &[u8],
) -> Outcome {
    let claimed: Value = match serde_json::from_slice(poll_body) {
        Ok(value) => value,
        Err(err) => {
            return Outcome::Failed {
                reason: format!("login response was not JSON: {err}"),
            };
        }
    };
    let field = |name: &str| {
        claimed
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let (Some(code), Some(verifier)) = (field("authorization_code"), field("code_verifier")) else {
        return Outcome::Failed {
            reason: "login response carried no authorization code".into(),
        };
    };

    let tokens = match exchange(&state.upstream, code, verifier).await {
        Ok(tokens) => tokens,
        Err(err) => {
            return Outcome::Failed {
                reason: format!("token exchange failed: {err}"),
            };
        }
    };
    install(state, provider_name, key_name, &tokens)
}

async fn exchange(
    client: &UpstreamClient,
    code: &str,
    verifier: &str,
) -> Result<Value, GatewayError> {
    let request = http::Request::post(subscription::CODEX_OAUTH_TOKEN_URL)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Body::from(subscription::codex_device_exchange_form(
            code, verifier,
        )))
        .map_err(|e| {
            GatewayError::new(
                ErrorClass::UpstreamError,
                format!("bad exchange request: {e}"),
            )
        })?;
    let response = send(client, request).await?;
    if !response.status.is_success() {
        // The body repeats the code we sent, so it is not logged.
        return Err(GatewayError::new(
            ErrorClass::Authentication,
            format!("token endpoint answered {}", response.status),
        ));
    }
    serde_json::from_slice(&response.bytes).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("token response was not JSON: {e}"),
        )
    })
}

/// Write the renewed credential and publish it onto the live seat.
///
/// The table is re-read here rather than captured at start: a login takes
/// as long as a person takes, and the config may have been reloaded
/// underneath it.
fn install(state: &Arc<AppState>, provider_name: &str, key_name: &str, tokens: &Value) -> Outcome {
    let table = state.table.load();
    let Some(key) = table
        .providers()
        .find(|p| p.name == provider_name)
        .and_then(|p| p.keys.iter().find(|k| k.name == key_name))
    else {
        return Outcome::Failed {
            reason: "the credential was removed while the login was in progress".into(),
        };
    };
    let (Some(seat), Some(path)) = (key.credential.seat(), key.source_path.as_deref()) else {
        return Outcome::Failed {
            reason: "the credential is no longer a seat with a file to write".into(),
        };
    };

    let current = seat.current();
    let merged =
        match credential::merge_refresh(current.document.expose(), tokens, vkey::unix_now_ms()) {
            Ok(merged) => merged,
            Err(err) => {
                return Outcome::Failed {
                    reason: format!("could not build the credential document: {err}"),
                };
            }
        };
    // Validated before it can replace a working document, exactly as a
    // refresh is.
    let renewed = match credential::parse_codex_auth_json(&merged) {
        Ok(renewed) => renewed,
        Err(err) => {
            return Outcome::Failed {
                reason: format!("the new credential did not parse: {err}"),
            };
        }
    };

    // The seat the operator clicked is a specific account. The browser
    // signs in as whoever is logged in there, which is not necessarily
    // the same one — and quietly writing account B's tokens over seat A
    // leaves two seats sharing one account and one quota, which reads as
    // a seat that mysteriously exhausts twice as fast.
    if let (Some(had), Some(got)) = (current.email.as_deref(), renewed.email.as_deref())
        && !had.eq_ignore_ascii_case(got)
    {
        return Outcome::Failed {
            reason: format!(
                "signed in as {got}, but this seat is {had} — sign out of ChatGPT, \
                 or use the seat for {got}"
            ),
        };
    }

    if let Err(err) = crate::refresh::write_atomic(path, &merged) {
        return Outcome::Failed {
            reason: format!("could not write the credential file: {err}"),
        };
    }
    let email = renewed.email.clone();
    seat.publish(renewed);
    tracing::info!(provider = %provider_name, key = %key_name, path, "seat signed back in");
    Outcome::Signed { email }
}

/// A response read to the end, which is all any of these calls need.
struct Read {
    status: StatusCode,
    bytes: bytes::Bytes,
}

async fn post_json(client: &UpstreamClient, url: &str, body: String) -> Result<Read, GatewayError> {
    let request = http::Request::post(url)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|e| GatewayError::new(ErrorClass::UpstreamError, format!("bad request: {e}")))?;
    send(client, request).await
}

async fn send(client: &UpstreamClient, request: http::Request<Body>) -> Result<Read, GatewayError> {
    let response = client
        .send(
            "codex-deviceauth",
            request,
            std::time::Duration::from_secs(30),
        )
        .await?;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| GatewayError::new(ErrorClass::UpstreamError, format!("body: {e}")))?
        .to_bytes();
    Ok(Read { status, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(provider: &str, key: &str, expires_at_ms: u64) -> DeviceLogin {
        DeviceLogin {
            provider: provider.into(),
            key: key.into(),
            user_code: "ABCD-EFGH".into(),
            verification_url: subscription::CODEX_DEVICE_VERIFICATION_URL.into(),
            expires_at_ms,
            outcome: Outcome::Waiting,
            finished_at_ms: None,
        }
    }

    #[test]
    fn one_outstanding_code_per_seat() {
        let registry = DeviceLoginRegistry::default();
        registry.insert("dl_1".into(), login("codex", "seat-a", 10_000));

        assert_eq!(
            registry
                .outstanding("codex", "seat-a", 5_000)
                .map(|(id, _)| id),
            Some("dl_1".into()),
            "a code still good is handed back, not replaced"
        );
        assert!(
            registry.outstanding("codex", "seat-b", 5_000).is_none(),
            "another seat's code is not this seat's"
        );
        assert!(
            registry.outstanding("codex", "seat-a", 20_000).is_none(),
            "an expired code is not offered again"
        );

        registry.settle("dl_1", Outcome::Signed { email: None }, 6_000);
        assert!(
            registry.outstanding("codex", "seat-a", 6_500).is_none(),
            "a finished login is not outstanding"
        );
    }

    #[test]
    fn finished_logins_survive_long_enough_to_be_read() {
        let registry = DeviceLoginRegistry::default();
        registry.insert("dl_1".into(), login("codex", "seat-a", 10_000));
        registry.settle(
            "dl_1",
            Outcome::Failed {
                reason: "nope".into(),
            },
            6_000,
        );

        registry.reap(6_000 + RETAIN_FINISHED_MS - 1);
        assert!(
            registry.get("dl_1").is_some(),
            "the console must still be able to read why it failed"
        );
        registry.reap(6_000 + RETAIN_FINISHED_MS);
        assert!(registry.get("dl_1").is_none());
    }

    #[test]
    fn an_abandoned_code_is_reaped_after_it_expires() {
        let registry = DeviceLoginRegistry::default();
        registry.insert("dl_1".into(), login("codex", "seat-a", 10_000));

        // Still outstanding: the operator may yet enter it.
        registry.reap(9_000);
        assert!(registry.get("dl_1").is_some());
        // Past expiry plus the grace window, nobody is coming back for it.
        registry.reap(10_000 + RETAIN_FINISHED_MS);
        assert!(registry.get("dl_1").is_none());
    }
}
