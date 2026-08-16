//! Sealed secrets: XChaCha20-Poly1305 under a data-encryption key.
//! Values are decrypted only in memory; the store and its backups carry
//! ciphertext.
//!
//! Where the key comes from is the whole design question, because every
//! node has to derive the *same* one. A key minted per node works for a
//! single box and silently breaks the moment a second node reads a secret
//! the first one sealed — it holds different bytes, so the unseal returns
//! `None` and a provider looks unconfigured.
//!
//! So a shared backend requires `RAPID_MASTER_KEY`: 32 bytes of base64
//! that the operator supplies to every task, from Secrets Manager, SSM,
//! or whatever their platform uses. Only the single-node file backend
//! falls back to minting a key on disk, where "every node" is one node
//! and there is nothing to disagree with.

use std::fs;
use std::io;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

/// Ciphertext + nonce, both base64. Safe to serialize, replicate, back up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedSecret {
    pub nonce: String,
    pub ct: String,
}

pub struct Sealer {
    cipher: XChaCha20Poly1305,
}

/// The environment variable holding the cluster-wide key.
pub const MASTER_KEY_ENV: &str = "RAPID_MASTER_KEY";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error(
        "{MASTER_KEY_ENV} is not set. A shared control-plane store needs one key across every \
         node, or secrets sealed by one node cannot be read by the others. Generate one with \
         `rapid-router master-key` and supply it to every node."
    )]
    Missing,
    #[error(
        "{MASTER_KEY_ENV} is not 32 bytes of base64. Generate one with `rapid-router master-key`."
    )]
    Malformed,
    #[error("reading the node key: {0}")]
    Io(#[from] io::Error),
}

#[derive(Serialize, Deserialize)]
struct NodeKeyFile {
    node_id: String,
    dek: String,
}

impl Sealer {
    /// Mint a fresh master key in the form the environment variable wants.
    pub fn generate_master_key() -> String {
        let mut key = [0u8; 32];
        fastrand::fill(&mut key);
        B64.encode(key)
    }

    /// The cluster-wide key from the environment. Required whenever more
    /// than one node can reach the store.
    pub fn from_env() -> Result<Self, KeyError> {
        let raw = std::env::var(MASTER_KEY_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(KeyError::Missing)?;
        Self::from_master_key(raw.trim())
    }

    pub fn from_master_key(b64: &str) -> Result<Self, KeyError> {
        let key = B64
            .decode(b64)
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or(KeyError::Malformed)?;
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(&key).expect("32-byte key"),
        })
    }

    /// Single-node fallback: the key lives beside the data, minted on
    /// first boot. Never used with a shared backend.
    pub fn load_or_create(path: &Path) -> io::Result<(Self, String)> {
        let parsed: NodeKeyFile = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("node.key: {e}"))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut dek = [0u8; 32];
                fastrand::fill(&mut dek);
                let fresh = NodeKeyFile {
                    node_id: uuid::Uuid::now_v7().to_string(),
                    dek: B64.encode(dek),
                };
                write_0600(
                    path,
                    &serde_json::to_vec(&fresh).expect("node key serializes"),
                )?;
                fresh
            }
            Err(e) => return Err(e),
        };
        let dek = B64
            .decode(&parsed.dek)
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "node.key: bad dek"))?;
        let cipher = XChaCha20Poly1305::new_from_slice(&dek).expect("32-byte key");
        Ok((Self { cipher }, parsed.node_id))
    }

    pub fn seal(&self, plaintext: &[u8]) -> SealedSecret {
        let mut nonce = [0u8; 24];
        fastrand::fill(&mut nonce);
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .expect("sealing cannot fail");
        SealedSecret {
            nonce: B64.encode(nonce),
            ct: B64.encode(ct),
        }
    }

    pub fn unseal(&self, sealed: &SealedSecret) -> Option<Vec<u8>> {
        let nonce = B64.decode(&sealed.nonce).ok()?;
        if nonce.len() != 24 {
            return None;
        }
        let ct = B64.decode(&sealed.ct).ok()?;
        self.cipher
            .decrypt(XNonce::from_slice(&nonce), ct.as_slice())
            .ok()
    }
}

#[cfg(unix)]
fn write_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    io::Write::write_all(&mut file, bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}
