//! Credentials that change under you.
//!
//! Every metered provider key in this gateway is a constant: it is read
//! once at config load and used until the config is replaced. A
//! subscription seat is not. Its access token expires — 10 days for Codex,
//! hours for Claude Code — and is renewed by an OAuth round trip that
//! **rotates the refresh token too**, so the old document becomes
//! unusable the moment the new one is issued.
//!
//! That single fact drives everything here:
//!
//! - The live value is behind an [`ArcSwap`], so a refresh publishes
//!   without blocking readers and no request ever sees a half-updated
//!   credential.
//! - A refresh **merges into the original document** rather than rewriting
//!   it, so fields we do not model (Codex's `auth_mode`, the CLI's own
//!   bookkeeping) survive a round trip and the file stays usable by the
//!   tool that created it.
//! - A merge that would produce a credential without an access token is
//!   refused, so a bad refresh response can never overwrite a good
//!   credential with junk.
//!
//! The OAuth request itself is not here — this module is pure, and the
//! transport lives at the edge with the other I/O.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use crate::secret::SecretString;

/// A seat credential as it currently stands.
///
/// Cheap to clone (it is what `ArcSwap` hands out) and carries no
/// interior mutability of its own: a refresh builds a whole new one.
#[derive(Debug)]
pub struct SeatState {
    /// What goes in the `Authorization` header.
    pub access_token: SecretString,
    /// Absent when the credential cannot renew itself — an inline token
    /// pasted into config, or a document that never carried one. Such a
    /// seat dies at expiry and needs an operator re-login.
    pub refresh_token: Option<SecretString>,
    /// ChatGPT account id, for the `ChatGPT-Account-Id` header. Codex
    /// only; `None` on every other credential shape.
    pub account_id: Option<String>,
    /// The account this seat signs in as, when the credential says so.
    ///
    /// Read once at parse time rather than on demand: it is the only
    /// human-readable handle a pool of eighty seats has, and decoding a
    /// JWT per console render to recover it would be absurd.
    pub email: Option<String>,
    /// Expiry in epoch milliseconds, when it can be determined — from a
    /// JWT `exp` claim (Codex) or an explicit field (Claude Code).
    /// `None` means "unknown", which is treated as "do not pre-emptively
    /// refresh", never as "expired".
    pub expires_at_ms: Option<u64>,
    /// The full document this was parsed from, so a refresh can merge.
    pub document: SecretString,
}

impl SeatState {
    /// Whether the access token is past its expiry at `now_ms`.
    ///
    /// Unknown expiry is **not** expired. Fail-open is deliberate: a
    /// credential shape we cannot date must not be removed from rotation
    /// on a guess, and a genuinely dead token surfaces as a `401` on its
    /// next use, which the reactive path handles.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|exp| now_ms >= exp)
    }

    /// Whether this should be renewed now, `skew_ms` ahead of expiry.
    ///
    /// A seat with no refresh token can never be renewed, so it never
    /// wants renewing — asking would burn an OAuth round trip per lease
    /// for a credential that cannot change.
    pub fn wants_refresh(&self, now_ms: u64, skew_ms: u64) -> bool {
        self.refresh_token.is_some()
            && self
                .expires_at_ms
                .is_some_and(|exp| now_ms.saturating_add(skew_ms) >= exp)
    }
}

/// A credential the gateway may have to renew.
#[derive(Debug)]
pub struct Seat {
    state: ArcSwap<SeatState>,
}

impl Seat {
    pub fn new(state: SeatState) -> Self {
        Self {
            state: ArcSwap::from_pointee(state),
        }
    }

    /// The credential as of right now. Readers take this once per request
    /// and use it throughout; a refresh landing mid-request does not
    /// change the token the request is already using.
    pub fn current(&self) -> Arc<SeatState> {
        self.state.load_full()
    }

    /// Publish a renewed credential. The previous value stays alive for
    /// any reader still holding it.
    pub fn publish(&self, state: SeatState) {
        self.state.store(Arc::new(state));
    }
}

/// What the gateway sends upstream, per key.
#[derive(Debug)]
pub enum Credential {
    /// A constant: every metered API key.
    Static(SecretString),
    /// A subscription seat's rotating OAuth credential.
    Seat(Arc<Seat>),
}

impl Credential {
    /// The bearer value to send on this request.
    ///
    /// Returns an owned `SecretString` rather than a borrow because a seat
    /// credential lives behind an `ArcSwap` and may be replaced while the
    /// caller is still building the request.
    pub fn token(&self) -> SecretString {
        match self {
            Credential::Static(secret) => secret.clone(),
            Credential::Seat(seat) => seat.current().access_token.clone(),
        }
    }

    pub fn seat(&self) -> Option<&Arc<Seat>> {
        match self {
            Credential::Seat(seat) => Some(seat),
            Credential::Static(_) => None,
        }
    }
}

/// Failure to make sense of a credential document.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CredentialError {
    #[error("credential is not valid JSON")]
    NotJson,
    #[error("credential document carries no access token")]
    NoAccessToken,
    #[error("codex credential carries no account id and no id_token to derive one from")]
    NoAccountId,
}

/// Parse a Codex CLI `auth.json`.
///
/// Both layouts the CLI has shipped are accepted: fields nested under
/// `tokens` (current) and the same fields at the top level (older). The
/// account id is taken from `account_id` when present, and otherwise
/// decoded from the `id_token`'s `chatgpt_account_id` claim — a real
/// credential in the wild may carry either.
///
/// Expiry comes from the access token's JWT `exp`; the document itself
/// does not record one. (Verified against a live credential: `exp` sits
/// exactly 10 days out, matching the `expires_in` the OAuth endpoint
/// returns.)
pub fn parse_codex_auth_json(document: &str) -> Result<SeatState, CredentialError> {
    let doc: Value = serde_json::from_str(document).map_err(|_| CredentialError::NotJson)?;
    let tokens = doc.get("tokens").unwrap_or(&Value::Null);
    let field = |name: &str| -> Option<&str> {
        tokens
            .get(name)
            .or_else(|| doc.get(name))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };

    let access_token = field("access_token").ok_or(CredentialError::NoAccessToken)?;
    let id_token = field("id_token");
    let account_id = field("account_id")
        .map(str::to_owned)
        .or_else(|| id_token.and_then(chatgpt_account_id))
        .ok_or(CredentialError::NoAccountId)?;

    Ok(SeatState {
        expires_at_ms: jwt_expiry_ms(access_token),
        access_token: SecretString::new(access_token.to_owned()),
        refresh_token: field("refresh_token").map(|t| SecretString::new(t.to_owned())),
        account_id: Some(account_id),
        email: id_token.and_then(jwt_email),
        document: SecretString::new(document.to_owned()),
    })
}

/// Parse a Claude Code credential document.
///
/// The shape is the one Claude Code stores (in the macOS keychain under
/// `Claude Code-credentials`, or in `~/.claude/.credentials.json`
/// elsewhere): an outer object with a `claudeAiOauth` member. The bare
/// inner object is accepted too, so an operator can lift just the part
/// that matters into their secret manager.
///
/// Unlike Codex, the access token is opaque (`sk-ant-oat01-…`), not a JWT
/// — expiry is only knowable from the document's own `expiresAt`, which
/// is in **milliseconds**.
pub fn parse_claude_oauth_json(document: &str) -> Result<SeatState, CredentialError> {
    let doc: Value = serde_json::from_str(document).map_err(|_| CredentialError::NotJson)?;
    let oauth = doc.get("claudeAiOauth").unwrap_or(&doc);
    let string = |name: &str| -> Option<&str> {
        oauth
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };

    let access_token = string("accessToken")
        .or_else(|| string("access_token"))
        .ok_or(CredentialError::NoAccessToken)?;

    Ok(SeatState {
        access_token: SecretString::new(access_token.to_owned()),
        refresh_token: string("refreshToken")
            .or_else(|| string("refresh_token"))
            .map(|t| SecretString::new(t.to_owned())),
        account_id: None,
        expires_at_ms: oauth
            .get("expiresAt")
            .or_else(|| oauth.get("expires_at"))
            .and_then(Value::as_u64),
        // Claude's document carries no account claim.
        email: None,
        document: SecretString::new(document.to_owned()),
    })
}

/// Wrap a bare token pasted into config as a seat that cannot renew.
///
/// Useful and honest: `claude setup-token` hands an operator a single
/// string, and there is nowhere for a refresh to be persisted anyway.
pub fn inline_token(token: &str) -> Result<SeatState, CredentialError> {
    if token.is_empty() {
        return Err(CredentialError::NoAccessToken);
    }
    Ok(SeatState {
        expires_at_ms: jwt_expiry_ms(token),
        access_token: SecretString::new(token.to_owned()),
        refresh_token: None,
        account_id: None,
        // Claude's document carries no account claim.
        email: None,
        document: SecretString::new(String::new()),
    })
}

/// Merge an OAuth token response into the document it renews.
///
/// Returns the new document, ready to be persisted and re-parsed. The
/// merge writes each renewed field in **both** the nested and top-level
/// positions, because a document may use either and a reader may look in
/// either — leaving a stale copy at the other position is how a
/// "successful" refresh gets silently undone on the next read.
///
/// Fields the OAuth response carries but the credential document does not
/// model (`expires_in`, `scope`, `earliest_refresh_at`, `oai_is`) are
/// deliberately dropped: the document is the CLI's format, expiry is
/// re-derived from the new token's own claim, and inventing fields in a
/// file another tool owns is how you break that tool.
///
/// Refuses a response with no access token, leaving the caller's existing
/// credential untouched.
pub fn merge_refresh(
    document: &str,
    response: &Value,
    now_ms: u64,
) -> Result<String, CredentialError> {
    let _access_token = response
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(CredentialError::NoAccessToken)?;

    let mut doc: Value = if document.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(document).map_err(|_| CredentialError::NotJson)?
    };
    let Some(root) = doc.as_object_mut() else {
        return Err(CredentialError::NotJson);
    };

    let renewed: Vec<(&str, String)> = ["access_token", "refresh_token", "id_token"]
        .into_iter()
        .filter_map(|name| {
            response
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|value| (name, value.to_owned()))
        })
        .collect();
    debug_assert!(renewed.iter().any(|(name, _)| *name == "access_token"));

    // Carry the account id forward: the renewed id_token may or may not
    // repeat it, and losing it costs the ChatGPT-Account-Id header.
    let existing_account_id = root
        .get("tokens")
        .and_then(|t| t.get("account_id"))
        .or_else(|| root.get("account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Written in BOTH positions, unconditionally. A live Codex auth.json
    // carries each token at the top level *and* under `tokens`, and either
    // may be the one a reader consults; updating only one leaves a stale
    // copy that silently undoes the refresh on the next read.
    let had_nested = root.get("tokens").is_some_and(Value::is_object);
    if had_nested && let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) {
        for (name, value) in &renewed {
            tokens.insert((*name).to_owned(), Value::String(value.clone()));
        }
    }
    for (name, value) in &renewed {
        root.insert((*name).to_owned(), Value::String(value.clone()));
    }

    let account_id = renewed
        .iter()
        .find(|(name, _)| *name == "id_token")
        .and_then(|(_, token)| chatgpt_account_id(token))
        .or(existing_account_id);
    if let Some(account_id) = account_id {
        if had_nested && let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) {
            tokens.insert("account_id".into(), Value::String(account_id.clone()));
        }
        root.insert("account_id".into(), Value::String(account_id));
    }
    root.insert(
        "last_refresh".into(),
        Value::String(rfc3339_utc_millis(now_ms)),
    );

    Ok(serde_json::to_string_pretty(&doc).expect("value serializes"))
}

/// The `exp` claim of a JWT, as epoch milliseconds.
///
/// Every failure mode returns `None` — "expiry unknown" — because this is
/// called while scanning candidate keys and must never be the thing that
/// takes a request down. That includes the arithmetic ones: a crafted or
/// corrupt `exp` can be `Infinity` (JSON accepts the literal) or an epoch
/// large enough to overflow milliseconds, and both must be rejected
/// rather than wrapped into a plausible-looking date.
pub fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?.as_f64()?;
    if !exp.is_finite() || exp <= 0.0 {
        return None;
    }
    let millis = exp * 1000.0;
    (millis <= u64::MAX as f64).then_some(millis as u64)
}

/// The ChatGPT account id carried in a Codex `id_token`.
///
/// It lives under a namespaced claim (`https://api.openai.com/auth`) and,
/// on some tokens, at the top level; both are checked.
/// The account email from an OpenAI id_token's claims.
fn jwt_email(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    claims
        .get("email")
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
        })
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn chatgpt_account_id(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Unpadded base64url, as JWTs use.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .ok()
}

/// `2026-08-15T17:46:15.000Z` — the `last_refresh` stamp, matching the
/// format the Codex CLI writes.
fn rfc3339_utc_millis(now_ms: u64) -> String {
    let secs = (now_ms / 1000) as i64;
    let millis = now_ms % 1000;
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60,
    )
}

/// Howard Hinnant's `civil_from_days`, the same one `vkey` uses for
/// budget periods.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a JWT-shaped token with the given claims. Signature is
    /// never checked — we only ever read claims from a token the provider
    /// gave us and is about to validate itself.
    fn jwt(claims: Value) -> String {
        use base64::Engine;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.to_string().as_bytes()),
            encode(b"not-a-real-signature")
        )
    }

    fn codex_document(exp_secs: u64) -> String {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": Value::Null,
            "tokens": {
                "id_token": jwt(json!({
                    "email": "seat@example.com",
                    "https://api.openai.com/auth": {"chatgpt_account_id": "acct-abc"}
                })),
                "access_token": jwt(json!({"exp": exp_secs})),
                "refresh_token": "rt.1.AADexample",
                "account_id": "acct-abc"
            },
            "last_refresh": "2026-08-09T04:52:55.974073Z"
        })
        .to_string()
    }

    #[test]
    fn codex_auth_json_parses_the_shipped_shape() {
        let state = parse_codex_auth_json(&codex_document(1_800_000_000)).unwrap();
        assert_eq!(state.account_id.as_deref(), Some("acct-abc"));
        assert!(state.refresh_token.is_some());
        assert_eq!(state.expires_at_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn codex_account_id_falls_back_to_the_id_token_claim() {
        let doc = json!({
            "tokens": {
                "access_token": jwt(json!({"exp": 1_800_000_000u64})),
                "id_token": jwt(json!({
                    "https://api.openai.com/auth": {"chatgpt_account_id": "from-claim"}
                }))
            }
        })
        .to_string();
        let state = parse_codex_auth_json(&doc).unwrap();
        assert_eq!(state.account_id.as_deref(), Some("from-claim"));
    }

    #[test]
    fn codex_flat_layout_still_parses() {
        let doc = json!({
            "access_token": jwt(json!({"exp": 1_800_000_000u64})),
            "account_id": "flat-acct"
        })
        .to_string();
        let state = parse_codex_auth_json(&doc).unwrap();
        assert_eq!(state.account_id.as_deref(), Some("flat-acct"));
        assert!(
            state.refresh_token.is_none(),
            "no refresh token in this document"
        );
    }

    #[test]
    fn codex_documents_missing_essentials_are_refused() {
        assert_eq!(
            parse_codex_auth_json("not json").unwrap_err(),
            CredentialError::NotJson
        );
        assert_eq!(
            parse_codex_auth_json(&json!({"tokens": {}}).to_string()).unwrap_err(),
            CredentialError::NoAccessToken
        );
        let no_account = json!({"tokens": {"access_token": "opaque"}}).to_string();
        assert_eq!(
            parse_codex_auth_json(&no_account).unwrap_err(),
            CredentialError::NoAccountId
        );
    }

    #[test]
    fn claude_credentials_parse_with_and_without_the_wrapper() {
        // The shape Claude Code stores, with the field names it uses.
        let inner = json!({
            "accessToken": "sk-ant-oat01-example",
            "refreshToken": "sk-ant-ort01-example",
            "expiresAt": 1_786_827_595_743u64,
            "scopes": ["user:inference"],
            "subscriptionType": "max"
        });
        let wrapped = json!({"claudeAiOauth": inner}).to_string();
        for document in [wrapped, inner.to_string()] {
            let state = parse_claude_oauth_json(&document).unwrap();
            assert_eq!(state.access_token.expose(), "sk-ant-oat01-example");
            assert!(state.refresh_token.is_some());
            assert_eq!(state.expires_at_ms, Some(1_786_827_595_743));
            assert_eq!(state.account_id, None, "Claude sends no account id");
        }
    }

    #[test]
    fn an_inline_token_can_never_refresh() {
        let state = inline_token("sk-ant-oat01-pasted").unwrap();
        assert!(state.refresh_token.is_none());
        assert!(
            !state.wants_refresh(u64::MAX / 2, 0),
            "nothing to refresh with"
        );
        assert_eq!(
            inline_token("").unwrap_err(),
            CredentialError::NoAccessToken
        );
    }

    #[test]
    fn expiry_is_fail_open_when_unknown() {
        let state = SeatState {
            email: None,
            access_token: SecretString::new("opaque".into()),
            refresh_token: None,
            account_id: None,
            expires_at_ms: None,
            document: SecretString::new(String::new()),
        };
        assert!(!state.is_expired(u64::MAX), "unknown expiry is not expired");
        assert!(!state.wants_refresh(u64::MAX, 120_000));
    }

    #[test]
    fn refresh_is_wanted_within_the_skew_window() {
        let state = parse_codex_auth_json(&codex_document(1_000_000)).unwrap();
        let expiry_ms = 1_000_000_000;
        assert!(!state.wants_refresh(expiry_ms - 300_000, 120_000));
        assert!(state.wants_refresh(expiry_ms - 60_000, 120_000));
        assert!(state.is_expired(expiry_ms));
    }

    #[test]
    fn merge_rewrites_both_positions_and_keeps_unmodelled_fields() {
        let original = codex_document(1_000_000);
        // The payload shape the live OAuth endpoint actually returns.
        let response = json!({
            "access_token": jwt(json!({"exp": 2_000_000u64})),
            "refresh_token": "rt.1.AABrotated",
            "id_token": jwt(json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-abc"}
            })),
            "token_type": "Bearer",
            "expires_in": 864_000,
            "earliest_refresh_at": 1_787_593_575u64,
            "oai_is": "ois1.opaque",
            "scope": "openid profile email"
        });
        let merged = merge_refresh(&original, &response, 1_786_815_918_000).unwrap();
        let doc: Value = serde_json::from_str(&merged).unwrap();

        // Both positions updated — a stale copy at either one silently
        // undoes the refresh on the next read.
        assert_eq!(doc["tokens"]["refresh_token"], "rt.1.AABrotated");
        assert_eq!(doc["refresh_token"], "rt.1.AABrotated");
        assert_eq!(doc["tokens"]["access_token"], doc["access_token"]);
        // Fields we do not model survive.
        assert_eq!(doc["auth_mode"], "chatgpt");
        // Fields the OAuth response carries but the document does not
        // model are not invented into it.
        assert!(doc.get("expires_in").is_none());
        assert!(doc.get("oai_is").is_none());
        assert_eq!(doc["tokens"]["account_id"], "acct-abc");
        assert!(
            doc["last_refresh"]
                .as_str()
                .unwrap()
                .starts_with("2026-08-15T")
        );

        // And the merged document re-parses into a usable credential.
        let state = parse_codex_auth_json(&merged).unwrap();
        assert_eq!(state.expires_at_ms, Some(2_000_000_000));
        assert_eq!(state.account_id.as_deref(), Some("acct-abc"));
    }

    #[test]
    fn merge_refuses_a_response_with_no_access_token() {
        let original = codex_document(1_000_000);
        let refused = merge_refresh(&original, &json!({"error": "invalid_grant"}), 0);
        assert_eq!(refused, Err(CredentialError::NoAccessToken));
        let empty = merge_refresh(&original, &json!({"access_token": ""}), 0);
        assert_eq!(empty, Err(CredentialError::NoAccessToken));
    }

    #[test]
    fn merge_into_an_empty_document_produces_a_usable_one() {
        let response = json!({
            "access_token": jwt(json!({"exp": 2_000_000u64})),
            "refresh_token": "rt.new",
            "id_token": jwt(json!({"chatgpt_account_id": "top-level-acct"}))
        });
        let merged = merge_refresh("", &response, 0).unwrap();
        let state = parse_codex_auth_json(&merged).unwrap();
        assert_eq!(state.account_id.as_deref(), Some("top-level-acct"));
        assert!(state.refresh_token.is_some());
    }

    #[test]
    fn jwt_expiry_refuses_corrupt_claims() {
        assert_eq!(jwt_expiry_ms("not-a-jwt"), None);
        assert_eq!(jwt_expiry_ms(&jwt(json!({}))), None);
        assert_eq!(jwt_expiry_ms(&jwt(json!({"exp": "soon"}))), None);
        assert_eq!(jwt_expiry_ms(&jwt(json!({"exp": -1}))), None);
        // An epoch large enough to overflow milliseconds must not wrap
        // into a plausible-looking date.
        assert_eq!(jwt_expiry_ms(&jwt(json!({"exp": 1e300}))), None);
    }

    #[test]
    fn publishing_a_refresh_does_not_disturb_a_reader() {
        let seat = Seat::new(parse_codex_auth_json(&codex_document(1_000_000)).unwrap());
        let in_flight = seat.current();
        let before = in_flight.access_token.expose().to_owned();

        seat.publish(inline_token("renewed-token").unwrap());

        assert_eq!(
            in_flight.access_token.expose(),
            before,
            "a request already holding a credential keeps using it"
        );
        assert_eq!(seat.current().access_token.expose(), "renewed-token");
    }

    #[test]
    fn credential_token_reads_through_the_seat() {
        let seat = Arc::new(Seat::new(inline_token("first").unwrap()));
        let credential = Credential::Seat(Arc::clone(&seat));
        assert_eq!(credential.token().expose(), "first");
        seat.publish(inline_token("second").unwrap());
        assert_eq!(credential.token().expose(), "second");

        let fixed = Credential::Static(SecretString::new("sk-static".into()));
        assert_eq!(fixed.token().expose(), "sk-static");
        assert!(fixed.seat().is_none());
    }

    #[test]
    fn refresh_stamp_is_a_real_utc_timestamp() {
        // 2026-08-15T17:46:15.000Z
        assert_eq!(
            rfc3339_utc_millis(1_786_852_575_000),
            "2026-08-16T03:56:15.000Z"
        );
        assert_eq!(rfc3339_utc_millis(0), "1970-01-01T00:00:00.000Z");
    }
}
