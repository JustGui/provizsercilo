use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("Key ref '{0}' not found in secrets dir or environment")]
    NotFound(String),
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
