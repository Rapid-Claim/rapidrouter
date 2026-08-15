use std::fmt;

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// A secret value that cannot leak through `Debug`, `Display`, or
/// serialization, and whose memory is zeroed on drop.
///
/// There is intentionally no `Serialize`/`Deserialize` implementation and no
/// way to move the inner `String` out; callers borrow via [`expose`] at the
/// point of use.
///
/// [`expose`]: SecretString::expose
#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Borrow the secret for immediate use (auth headers, verification).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time equality against a candidate value.
    ///
    /// Comparison time depends only on the candidate's length, never on how
    /// many bytes match.
    pub fn verify(&self, candidate: &str) -> bool {
        let ours = self.0.as_bytes();
        let theirs = candidate.as_bytes();
        if ours.len() != theirs.len() {
            // Burn the same comparison work before rejecting.
            let _ = theirs.ct_eq(theirs);
            return false;
        }
        ours.ct_eq(theirs).into()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact() {
        let s = SecretString::new("sk-very-secret".into());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn verify_matches_only_exact_value() {
        let s = SecretString::new("sk-abc123".into());
        assert!(s.verify("sk-abc123"));
        assert!(!s.verify("sk-abc124"));
        assert!(!s.verify("sk-abc12"));
        assert!(!s.verify(""));
    }

    #[test]
    fn expose_returns_inner() {
        let s = SecretString::new("value".into());
        assert_eq!(s.expose(), "value");
    }
}
