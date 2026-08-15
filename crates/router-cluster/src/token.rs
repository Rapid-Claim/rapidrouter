//! Join tokens: one string that both authorizes membership and pins the
//! cluster's identity.
//!
//! `caret-join-1.<base64url cluster-id>.<base64url secret>` — a bearer
//! credential, because that is what the job actually needs: a node
//! joining for the first time has no shared state, so whatever the
//! operator copies to it must be sufficient on its own to authenticate.
//! The cluster id rides along so a token pasted at the wrong fleet fails
//! with "that token is for cluster X" instead of a bare 401.
//!
//! Comparison is constant-time on both halves. The token is a secret:
//! it never appears in `Debug` output or logs, and it is only ever sent
//! over the cluster port, which belongs on an internal network.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
const PREFIX: &str = "caret-join-1.";

/// A cluster's shared credential. Held in the store, printed on demand,
/// never logged.
#[derive(Clone)]
pub struct JoinToken {
    cluster: String,
    secret: [u8; 32],
}

impl std::fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinToken")
            .field("cluster", &self.cluster)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("not a caret join token")]
    Malformed,
    #[error("token is for cluster `{0}`, this cluster is `{1}`")]
    WrongCluster(String, String),
    #[error("token signature does not verify")]
    BadSignature,
}

impl JoinToken {
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        fastrand::fill(&mut secret);
        let mut cluster = [0u8; 8];
        fastrand::fill(&mut cluster);
        Self {
            cluster: hex(&cluster),
            secret,
        }
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster
    }

    /// Reconstruct from the persisted form (cluster id + base64 secret).
    pub fn from_parts(cluster: String, secret_b64: &str) -> Option<Self> {
        let bytes = B64
            .decode(secret_b64)
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(secret_b64))
            .ok()?;
        Some(Self {
            cluster,
            secret: bytes.try_into().ok()?,
        })
    }

    pub fn secret_b64(&self) -> String {
        B64.encode(self.secret)
    }

    /// The operator-facing string. This is secret material.
    pub fn encode(&self) -> String {
        format!(
            "{PREFIX}{}.{}",
            B64.encode(self.cluster.as_bytes()),
            B64.encode(self.secret)
        )
    }

    /// Verify a presented token against this cluster's credential.
    pub fn verify(&self, presented: &str) -> Result<(), TokenError> {
        let rest = presented
            .strip_prefix(PREFIX)
            .ok_or(TokenError::Malformed)?;
        let (cluster_b64, secret_b64) = rest.split_once('.').ok_or(TokenError::Malformed)?;
        let cluster = B64.decode(cluster_b64).map_err(|_| TokenError::Malformed)?;
        let secret = B64.decode(secret_b64).map_err(|_| TokenError::Malformed)?;

        // Compare the secret first and in constant time, so a wrong-cluster
        // message is only ever produced for someone who already holds the
        // credential.
        let secret_ok =
            secret.len() == self.secret.len() && bool::from(subtle_ct_eq(&secret, &self.secret));
        if !secret_ok {
            return Err(TokenError::BadSignature);
        }
        let cluster = String::from_utf8(cluster).map_err(|_| TokenError::Malformed)?;
        if !bool::from(subtle_ct_eq(cluster.as_bytes(), self.cluster.as_bytes())) {
            return Err(TokenError::WrongCluster(cluster, self.cluster.clone()));
        }
        Ok(())
    }

    /// Parse a token an operator supplied: this is how a joining node
    /// learns both the cluster identity and the credential.
    pub fn parse(presented: &str) -> Option<Self> {
        let rest = presented.strip_prefix(PREFIX)?;
        let (cluster_b64, secret_b64) = rest.split_once('.')?;
        let cluster = String::from_utf8(B64.decode(cluster_b64).ok()?).ok()?;
        let secret: [u8; 32] = B64.decode(secret_b64).ok()?.try_into().ok()?;
        Some(Self { cluster, secret })
    }
}

fn subtle_ct_eq(a: &[u8], b: &[u8]) -> subtle::Choice {
    use subtle::ConstantTimeEq;
    a.ct_eq(b)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_verifies() {
        let token = JoinToken::generate();
        let encoded = token.encode();
        assert!(encoded.starts_with(PREFIX));
        assert_eq!(token.verify(&encoded), Ok(()));
    }

    #[test]
    fn a_token_from_another_cluster_is_rejected() {
        let ours = JoinToken::generate();
        let theirs = JoinToken::generate();
        // Different secret entirely: the MAC fails before the cluster id
        // is even trusted.
        assert_eq!(ours.verify(&theirs.encode()), Err(TokenError::BadSignature));
    }

    #[test]
    fn same_secret_different_cluster_id_is_named_precisely() {
        let ours = JoinToken::generate();
        let renamed = JoinToken {
            cluster: "deadbeef".into(),
            secret: ours.secret,
        };
        match ours.verify(&renamed.encode()) {
            Err(TokenError::WrongCluster(theirs, mine)) => {
                assert_eq!(theirs, "deadbeef");
                assert_eq!(mine, ours.cluster);
            }
            other => panic!("expected a cluster mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_single_flipped_bit_in_the_secret_is_rejected() {
        let token = JoinToken::generate();
        let mut secret = token.secret;
        secret[7] ^= 0x01;
        let forged = JoinToken {
            cluster: token.cluster.clone(),
            secret,
        };
        assert_eq!(
            token.verify(&forged.encode()),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_joining_node_can_parse_the_token_it_was_given() {
        let token = JoinToken::generate();
        let parsed = JoinToken::parse(&token.encode()).expect("parses");
        assert_eq!(parsed.cluster_id(), token.cluster_id());
        // And the parsed copy authenticates against the original cluster.
        assert_eq!(token.verify(&parsed.encode()), Ok(()));
        assert!(JoinToken::parse("not-a-token").is_none());
    }

    #[test]
    fn malformed_shapes_are_rejected_not_panicked() {
        let token = JoinToken::generate();
        for bad in [
            "",
            "nope",
            "caret-join-1.",
            "caret-join-1.abc",
            "caret-join-1.!!.!!",
        ] {
            assert!(token.verify(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn debug_output_never_reveals_the_secret() {
        let token = JoinToken::generate();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(&token.secret_b64()));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn persisted_parts_round_trip() {
        let token = JoinToken::generate();
        let restored =
            JoinToken::from_parts(token.cluster_id().to_owned(), &token.secret_b64()).unwrap();
        assert_eq!(restored.verify(&token.encode()), Ok(()));
    }
}
