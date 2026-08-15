//! Sealed secrets: XChaCha20-Poly1305 under a data-encryption key minted
//! at first boot (`node.key`, mode 0600). Values are decrypted only in
//! memory; the store, its backups, and the replication stream carry
//! ciphertext.

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

#[derive(Serialize, Deserialize)]
struct NodeKeyFile {
    node_id: String,
    dek: String,
}

impl Sealer {
    /// Load the node key, creating one on first boot. Returns the sealer
    /// and the node's stable identity.
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
