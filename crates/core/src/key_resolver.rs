use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;
use thiserror::Error;

/// `key_ref` prefix of a key value stored encrypted in the DB instead of named by
/// an env var / secrets file. Used only for keys sercilo creates itself through a
/// vendor API (per-tenant keys), whose value the vendor shows once.
pub const ENCRYPTED_PREFIX: &str = "enc:v1:";
/// Env var / secrets file holding the base64 32-byte AES-256-GCM master key.
pub const ENCRYPTION_KEY_REF: &str = "SERCILO_KEY_ENCRYPTION_KEY";

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("Key ref '{0}' not found in secrets dir or environment")]
    NotFound(String),
    #[error("Encrypted key could not be decrypted (missing or wrong {ENCRYPTION_KEY_REF})")]
    Decrypt,
    #[error("Failed to read secrets file for '{key_ref}': {source}")]
    FileRead {
        key_ref: String,
        #[source]
        source: std::io::Error,
    },
}

impl ResolveError {
    pub fn checked_locations(&self, key_ref: &str, secrets_dir: &Path) -> Vec<String> {
        vec![
            format!("file:{}", secrets_dir.join(key_ref).display()),
            format!("env:{key_ref}"),
        ]
    }
}

/// Resolve a key_ref to its actual value.
///
/// Resolution order:
///   1. `$SECRETS_DIR/<key_ref>` - Docker secrets file
///   2. `std::env::var(key_ref)` - environment variable
///
/// The key value is never logged or stored beyond the duration of the caller's use.
pub fn resolve_key(key_ref: &str, secrets_dir: &Path) -> Result<String, ResolveError> {
    if let Some(sealed) = key_ref.strip_prefix(ENCRYPTED_PREFIX) {
        return open(sealed, &master_key(secrets_dir)?);
    }
    resolve_plain(key_ref, secrets_dir)
}

fn resolve_plain(key_ref: &str, secrets_dir: &Path) -> Result<String, ResolveError> {
    let file = secrets_dir.join(key_ref);
    if file.exists() {
        let value = std::fs::read_to_string(&file).map_err(|e| ResolveError::FileRead {
            key_ref: key_ref.to_string(),
            source: e,
        })?;
        return Ok(value.trim().to_string());
    }
    std::env::var(key_ref).map_err(|_| ResolveError::NotFound(key_ref.to_string()))
}

/// Returns ("ok", None) or ("missing", Some(checked_locations)).
pub fn check_key(key_ref: &str, secrets_dir: &Path) -> (bool, Option<Vec<String>>) {
    match resolve_key(key_ref, secrets_dir) {
        Ok(_) => (true, None),
        Err(e) => {
            let locations = e.checked_locations(key_ref, secrets_dir);
            (false, Some(locations))
        }
    }
}

fn master_key(secrets_dir: &Path) -> Result<LessSafeKey, ResolveError> {
    let raw = resolve_plain(ENCRYPTION_KEY_REF, secrets_dir).map_err(|_| ResolveError::Decrypt)?;
    let bytes = B64.decode(raw.trim()).map_err(|_| ResolveError::Decrypt)?;
    let key = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| ResolveError::Decrypt)?;
    Ok(LessSafeKey::new(key))
}

fn open(sealed: &str, key: &LessSafeKey) -> Result<String, ResolveError> {
    let mut data = B64.decode(sealed).map_err(|_| ResolveError::Decrypt)?;
    if data.len() <= NONCE_LEN {
        return Err(ResolveError::Decrypt);
    }
    let mut body = data.split_off(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(&data).map_err(|_| ResolveError::Decrypt)?;
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut body)
        .map_err(|_| ResolveError::Decrypt)?;
    String::from_utf8(plain.to_vec()).map_err(|_| ResolveError::Decrypt)
}

/// Encrypt a vendor key value into a `key_ref` (`enc:v1:<base64 nonce|ciphertext>`).
pub fn seal_key(value: &str, secrets_dir: &Path) -> Result<String, ResolveError> {
    let key = master_key(secrets_dir)?;
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| ResolveError::Decrypt)?;
    let mut body = value.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut body)
        .map_err(|_| ResolveError::Decrypt)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&body);
    Ok(format!("{ENCRYPTED_PREFIX}{}", B64.encode(out)))
}

/// What admin listings show instead of a sealed `key_ref`.
pub fn display_ref(key_ref: &str) -> &str {
    if key_ref.starts_with(ENCRYPTED_PREFIX) {
        "enc:v1:<sealed>"
    } else {
        key_ref
    }
}

#[cfg(test)]
mod sealed_tests {
    use super::*;

    #[test]
    fn seal_round_trips_and_needs_the_master_key() {
        let dir = std::env::temp_dir().join(format!("sercilo-seal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(ENCRYPTION_KEY_REF), B64.encode([7u8; 32])).unwrap();
        let sealed = seal_key("stn_secret", &dir).unwrap();
        assert!(sealed.starts_with(ENCRYPTED_PREFIX) && !sealed.contains("stn_secret"));
        assert_eq!(resolve_key(&sealed, &dir).unwrap(), "stn_secret");
        assert_ne!(
            seal_key("stn_secret", &dir).unwrap(),
            sealed,
            "fresh nonce per seal"
        );

        std::fs::write(dir.join(ENCRYPTION_KEY_REF), B64.encode([8u8; 32])).unwrap();
        assert!(matches!(
            resolve_key(&sealed, &dir),
            Err(ResolveError::Decrypt)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
