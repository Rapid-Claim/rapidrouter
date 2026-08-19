//! Renewing a subscription seat's credential.
//!
//! A Codex access token lives ten days and is renewed by an OAuth round
//! trip that **rotates the refresh token with it**. That rotation is what
//! makes this delicate: the moment the endpoint answers, the old refresh
//! token is dead, and if the new one is not durably recorded the seat can
//! never be renewed again — an operator has to log in by hand.
//!
//! Everything here follows from that:
//!
//! - **Single-flight per seat.** Two concurrent refreshes would each
//!   rotate the token and one would land second, leaving the loser's
//!   credential recorded and the winner's dead.
//! - **Persist before publish.** The renewed document is written to disk
//!   first; only then does it become the live credential. A crash between
//!   the two costs one wasted refresh, which is recoverable. The reverse
//!   order costs the seat.
//! - **A failed refresh changes nothing.** The existing credential stays
//!   live and the caller falls back to normal error handling, because a
//!   token that has not yet expired usually still works.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use http_body_util::BodyExt;
use router_core::config::ProviderKind;
use router_core::credential::{self, Seat};
use router_core::router::KeyRuntime;
use router_core::{ErrorClass, GatewayError};
use router_providers::subscription;

use crate::upstream::UpstreamClient;

/// How far ahead of expiry a credential is renewed.
///
/// Two minutes covers a slow OAuth round trip plus the request the token
/// is about to be used for, without renewing so eagerly that a seat burns
/// refreshes it does not need.
pub const REFRESH_SKEW_MS: u64 = 2 * 60 * 1000;

/// Seats currently being renewed, so concurrent requests wait on one
/// round trip instead of racing to rotate the same token.
///
/// Keyed by the destination the credential is persisted to, which is what
/// actually collides — two providers configured against one `auth.json`
/// are one seat wearing two names.
#[derive(Default)]
pub struct RefreshRegistry {
    in_flight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl RefreshRegistry {
    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut in_flight = self.in_flight.lock().expect("registry mutex");
        Arc::clone(
            in_flight
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

/// Where a renewed credential is persisted.
///
/// A seat configured from an environment variable or an inline value has
/// nowhere to write, and renewing it would succeed upstream while changing
/// nothing on re-read — every lease would burn an OAuth call for a
/// credential that stays stale. Those seats are not renewed at all.
#[derive(Debug, Clone)]
pub enum Persist {
    File(String),
    /// No durable home: refresh is refused rather than silently wasted.
    None,
}

impl Persist {
    /// Read the destination out of a configured key value.
    pub fn from_ref(value: &str) -> Self {
        match value.strip_prefix("file:") {
            Some(path) if !path.is_empty() => Self::File(path.to_owned()),
            _ => Self::None,
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::File(path) => path,
            Self::None => "",
        }
    }
}

/// Renew a seat if it is close enough to expiry to need it.
///
/// Returns `true` when a renewal actually happened. A seat that does not
/// need renewing, cannot be renewed, or fails to renew all return `false`
/// — none of which is an error at the call site, because an unexpired
/// token is still worth trying.
pub async fn refresh_if_stale(
    client: &UpstreamClient,
    registry: &RefreshRegistry,
    kind: ProviderKind,
    key: &KeyRuntime,
    persist: &Persist,
    now_ms: u64,
) -> bool {
    let Some(seat) = key.seat() else {
        return false;
    };
    if !seat.current().wants_refresh(now_ms, REFRESH_SKEW_MS) {
        return false;
    }
    refresh_now(client, registry, kind, seat, persist, now_ms).await
}

/// Renew a seat unconditionally — the reactive path, after a `401`.
pub async fn refresh_now(
    client: &UpstreamClient,
    registry: &RefreshRegistry,
    kind: ProviderKind,
    seat: &Arc<Seat>,
    persist: &Persist,
    now_ms: u64,
) -> bool {
    let Persist::File(path) = persist else {
        tracing::debug!("seat credential has no durable home; not refreshing");
        return false;
    };
    // Codex is the only transport whose refresh flow is implemented. A
    // Claude subscription token is renewed by its own CLI, and guessing at
    // that endpoint would burn the credential we are trying to save.
    if kind != ProviderKind::CodexSubscription {
        return false;
    }

    let gate = registry.gate(persist.id());
    let _held = gate.lock().await;

    // Re-check under the gate: while we waited, the winner of the race may
    // have already renewed this seat, and refreshing again would rotate a
    // perfectly good token for nothing.
    let state = seat.current();
    if !state.wants_refresh(now_ms, REFRESH_SKEW_MS) && !state.is_expired(now_ms) {
        return false;
    }
    let Some(refresh_token) = &state.refresh_token else {
        return false;
    };

    let response = match post_refresh(client, refresh_token.expose()).await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "seat credential refresh failed; keeping the current one");
            return false;
        }
    };

    let merged = match credential::merge_refresh(state.document.expose(), &response, now_ms) {
        Ok(merged) => merged,
        Err(err) => {
            tracing::warn!(error = %err, "refresh response was unusable; credential left untouched");
            return false;
        }
    };
    // Validate before persisting: a document that will not parse back into
    // a usable credential must never replace one that does.
    let renewed = match credential::parse_codex_auth_json(&merged) {
        Ok(renewed) => renewed,
        Err(err) => {
            tracing::warn!(error = %err, "refreshed credential did not re-parse; not persisting");
            return false;
        }
    };
    if let Err(err) = write_atomic(path, &merged) {
        // Publishing a credential we could not record would strand the
        // seat on the next restart: the rotated refresh token would exist
        // only in memory, and the file would hold a dead one.
        tracing::error!(error = %err, path, "could not persist refreshed credential; not publishing");
        return false;
    }
    seat.publish(renewed);
    tracing::info!(path, "renewed subscription seat credential");
    true
}

async fn post_refresh(
    client: &UpstreamClient,
    refresh_token: &str,
) -> Result<serde_json::Value, GatewayError> {
    let request = http::Request::post(subscription::CODEX_OAUTH_TOKEN_URL)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Body::from(subscription::codex_refresh_form(refresh_token)))
        .map_err(|e| {
            GatewayError::new(
                ErrorClass::UpstreamError,
                format!("bad refresh request: {e}"),
            )
        })?;

    let response = client
        .send("codex-oauth", request, std::time::Duration::from_secs(30))
        .await?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| GatewayError::new(ErrorClass::UpstreamError, format!("refresh body: {e}")))?
        .to_bytes();
    if !status.is_success() {
        // The body carries the reason (`invalid_refresh_token` for a
        // credential that has been revoked or already rotated) but also
        // the token we sent, so it is not logged verbatim.
        return Err(GatewayError::new(
            ErrorClass::Authentication,
            format!("credential refresh refused with {status}"),
        ));
    }
    serde_json::from_slice(&body).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("refresh response was not JSON: {e}"),
        )
    })
}

/// Write via a temporary file in the same directory, then rename.
///
/// Shared with [`crate::device_login`], which installs a credential the
/// same way and must not invent a second answer to "how is this written".
///
/// The rename is atomic within a filesystem, so a reader — the vendor's
/// own CLI, or another node — never observes a half-written credential.
/// The temporary file is created alongside the target rather than in
/// `/tmp` precisely so the rename cannot cross a filesystem boundary and
/// silently degrade into a copy.
pub(crate) fn write_atomic(path: &str, contents: &str) -> std::io::Result<()> {
    let target = std::path::Path::new(path);
    let directory = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    let temporary = directory.join(format!(
        ".{}.rapid-tmp",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "credential".into())
    ));
    std::fs::write(&temporary, contents)?;
    restrict_permissions(&temporary)?;
    std::fs::rename(&temporary, target)
}

/// Credentials are owner-readable only. The vendor CLIs write them `0600`
/// and a rename would otherwise install our default-permission file in
/// place of theirs.
#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_targets_come_from_file_refs_only() {
        assert!(matches!(
            Persist::from_ref("file:/etc/rapid/auth.json"),
            Persist::File(path) if path == "/etc/rapid/auth.json"
        ));
        // An env or inline credential has nowhere to write a rotated
        // token, so it must not be refreshed at all.
        assert!(matches!(Persist::from_ref("env.CODEX_AUTH"), Persist::None));
        assert!(matches!(Persist::from_ref("file:"), Persist::None));
        assert!(matches!(Persist::from_ref("sk-ant-oat01-x"), Persist::None));
    }

    #[test]
    fn atomic_write_replaces_and_restricts() {
        let directory = std::env::temp_dir().join(format!("rapid-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("auth.json");
        std::fs::write(&path, "{\"old\": true}").unwrap();

        write_atomic(path.to_str().unwrap(), "{\"new\": true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\": true}");
        assert!(
            !directory.join(".auth.json.rapid-tmp").exists(),
            "the temporary file is renamed away, not left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials stay owner-only");
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    #[tokio::test]
    async fn the_same_seat_is_gated_by_one_lock() {
        let registry = RefreshRegistry::default();
        let first = registry.gate("/path/auth.json");
        let second = registry.gate("/path/auth.json");
        let other = registry.gate("/other/auth.json");
        assert!(Arc::ptr_eq(&first, &second), "one seat, one round trip");
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
