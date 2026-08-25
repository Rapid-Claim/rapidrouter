//! Console session tokens that survive a gateway restart.
//!
//! Sessions used to live only in a `HashMap` on `AppState`, which made a
//! restart sign everybody out. That is not merely inconvenient: the
//! console holds its token in `sessionStorage`, so after a restart the tab
//! still believes it is authenticated, the SSE stream 401s, and the header
//! sits on "Reconnecting" forever without ever saying why. A deploy
//! shouldn't look like a network fault.
//!
//! So a token carries its own claims and a signature over them, and the
//! in-memory map becomes a cache rather than the source of truth. A
//! restart loses the cache and re-derives the session from the token the
//! browser already holds.
//!
//! Where the signing key comes from is the same question `router_store`'s
//! sealer answers, and it gets the same answer:
//!
//! - `RAPID_MASTER_KEY` when set, so every node in a fleet derives the
//!   same key and a session follows a caller across nodes;
//! - otherwise a key minted beside the data on first boot, where "every
//!   node" is one node and there is nothing to disagree with;
//! - otherwise — no data directory at all — a per-process key, which is
//!   exactly today's behaviour and the only honest option when there is
//!   nowhere to persist anything.
//!
//! The key is *derived* from the master key rather than being it, so a
//! console token can never be mistaken for, or used to attack, a sealed
//! secret.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::Principal;

/// Domain separation, so this key cannot collide with any other use of
/// the master key.
const DERIVATION_CONTEXT: &[u8] = b"rapid-router/console-session/v1";

/// Tokens start with this, so an old-format token is recognisably old
/// rather than a signature failure.
const PREFIX: &str = "cs1.";

const KEY_FILE: &str = "console-session.key";

/// What a token asserts. Kept small: it travels in a cookie on every
/// admin request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    /// `a` for the static admin key, or a user id.
    #[serde(rename = "s")]
    subject: String,
    /// Unix milliseconds.
    #[serde(rename = "e")]
    expires_ms: u64,
    /// Random, so two sessions minted in the same millisecond for the
    /// same principal are still distinct tokens.
    #[serde(rename = "n")]
    nonce: u64,
}

const ADMIN_KEY_SUBJECT: &str = "a";

impl Claims {
    fn principal(&self) -> Principal {
        if self.subject == ADMIN_KEY_SUBJECT {
            Principal::AdminKey
        } else {
            Principal::User {
                id: self.subject.clone(),
            }
        }
    }
}

/// Mints and verifies console session tokens.
pub struct SessionSigner {
    key: [u8; 32],
    /// False when the key is per-process, so a restart really will sign
    /// everyone out. Reported at boot rather than discovered later.
    durable: bool,
}

impl SessionSigner {
    /// Resolve the signing key from the environment, then from disk, then
    /// from nothing.
    pub fn resolve(data_dir: Option<&Path>) -> Self {
        if let Ok(master) = std::env::var(router_store::MASTER_KEY_ENV)
            && let Some(bytes) = B64.decode(master.trim()).ok().filter(|b| b.len() == 32)
        {
            return Self {
                key: derive(&bytes),
                durable: true,
            };
        }
        if let Some(dir) = data_dir
            && let Some(seed) = load_or_create_seed(&dir.join(KEY_FILE))
        {
            return Self {
                key: derive(&seed),
                durable: true,
            };
        }
        let mut seed = [0u8; 32];
        fastrand::fill(&mut seed);
        Self {
            key: derive(&seed),
            durable: false,
        }
    }

    /// True when a token minted now will still verify after a restart.
    pub fn is_durable(&self) -> bool {
        self.durable
    }

    pub fn mint(&self, principal: &Principal, expires_ms: u64) -> String {
        let claims = Claims {
            subject: match principal {
                Principal::AdminKey => ADMIN_KEY_SUBJECT.to_owned(),
                Principal::User { id } => id.clone(),
            },
            expires_ms,
            nonce: fastrand::u64(..),
        };
        let payload = serde_json::to_vec(&claims).expect("claims serialize");
        let body = B64URL.encode(&payload);
        let signature = B64URL.encode(self.sign(body.as_bytes()));
        format!("{PREFIX}{body}.{signature}")
    }

    /// The principal this token proves, or `None` if it is malformed,
    /// forged, or expired.
    pub fn verify(&self, token: &str, now_ms: u64) -> Option<(Principal, u64)> {
        let rest = token.strip_prefix(PREFIX)?;
        let (body, signature) = rest.split_once('.')?;
        let presented = B64URL.decode(signature).ok()?;
        // Constant time: a byte-by-byte compare leaks how much of a
        // forged signature was right, which is enough to build one.
        if !bool::from(presented.ct_eq(&self.sign(body.as_bytes()))) {
            return None;
        }
        let claims: Claims = serde_json::from_slice(&B64URL.decode(body).ok()?).ok()?;
        if claims.expires_ms < now_ms {
            return None;
        }
        Some((claims.principal(), claims.expires_ms))
    }

    fn sign(&self, body: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.key).expect("hmac accepts any key");
        mac.update(body);
        mac.finalize().into_bytes().into()
    }
}

fn derive(seed: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(seed).expect("hmac accepts any key");
    mac.update(DERIVATION_CONTEXT);
    mac.finalize().into_bytes().into()
}

/// Read the seed beside the data, minting it on first boot.
///
/// A read failure returns `None` rather than panicking: an unreadable key
/// file should degrade to per-process sessions, not stop the gateway
/// serving traffic.
fn load_or_create_seed(path: &Path) -> Option<[u8; 32]> {
    if let Ok(text) = std::fs::read_to_string(path)
        && let Some(bytes) = B64.decode(text.trim()).ok().filter(|b| b.len() == 32)
    {
        return Some(bytes.try_into().expect("checked length"));
    }
    let mut seed = [0u8; 32];
    fastrand::fill(&mut seed);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_private(path, B64.encode(seed).as_bytes()).ok()?;
    Some(seed)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> SessionSigner {
        SessionSigner {
            key: derive(b"a fixed seed for tests"),
            durable: true,
        }
    }

    #[test]
    fn a_token_survives_the_process_that_minted_it() {
        let minted = signer().mint(&Principal::User { id: "u1".into() }, 10_000);
        // A second signer with the same seed is what a restart looks like.
        let (principal, expires) = signer().verify(&minted, 9_999).expect("verifies");
        assert!(matches!(principal, Principal::User { id } if id == "u1"));
        assert_eq!(expires, 10_000);
    }

    #[test]
    fn an_expired_token_is_refused_even_though_it_is_signed() {
        let minted = signer().mint(&Principal::AdminKey, 10_000);
        assert!(signer().verify(&minted, 10_001).is_none());
    }

    #[test]
    fn a_token_signed_by_another_key_does_not_verify() {
        let minted = signer().mint(&Principal::AdminKey, u64::MAX);
        let stranger = SessionSigner {
            key: derive(b"a different seed"),
            durable: true,
        };
        assert!(stranger.verify(&minted, 0).is_none());
    }

    #[test]
    fn tampering_with_the_claims_invalidates_the_signature() {
        let minted = signer().mint(&Principal::User { id: "u1".into() }, u64::MAX);
        let (body, signature) = minted
            .strip_prefix(PREFIX)
            .unwrap()
            .split_once('.')
            .unwrap();
        let mut claims: Claims = serde_json::from_slice(&B64URL.decode(body).unwrap()).unwrap();
        claims.subject = "u2".into();
        let forged = format!(
            "{PREFIX}{}.{signature}",
            B64URL.encode(serde_json::to_vec(&claims).unwrap()),
        );
        assert!(signer().verify(&forged, 0).is_none());
    }

    #[test]
    fn garbage_is_refused_rather_than_panicking() {
        for bad in ["", "cs1.", "cs1.x", "cs1.x.y", "nonsense", "cs_legacytoken"] {
            assert!(signer().verify(bad, 0).is_none(), "{bad} should not verify");
        }
    }

    #[test]
    fn two_sessions_for_one_principal_are_distinct_tokens() {
        let s = signer();
        assert_ne!(
            s.mint(&Principal::AdminKey, 10_000),
            s.mint(&Principal::AdminKey, 10_000),
        );
    }

    #[test]
    fn a_seed_on_disk_is_reused_rather_than_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(KEY_FILE);
        let first = load_or_create_seed(&path).expect("minted");
        let second = load_or_create_seed(&path).expect("reread");
        assert_eq!(first, second);
    }
}
