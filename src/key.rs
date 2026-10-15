//! Resolves the agent37 API key without ever letting it into crabbox config.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zeroize::Zeroizing;

/// The agent37 API key. Wiped from memory on drop; never printed.
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// The raw key, for building the authorization header only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Picks the key from `config.keyFile` first, then `env_key`.
///
/// A key inside `config` is rejected outright: crabbox config is logged and
/// shared, so the key must come from the environment or a private file.
pub fn resolve(config: &serde_json::Value, env_key: Option<String>) -> Result<ApiKey, String> {
    if config.get("apiKey").is_some() || config.get("api_key").is_some() {
        return Err(
            "config must not carry the API key; set AGENT37_API_KEY or config.keyFile".to_owned(),
        );
    }
    if let Some(path) = config.get("keyFile").and_then(serde_json::Value::as_str) {
        return read_key_file(Path::new(path));
    }
    let env_key = Zeroizing::new(env_key.unwrap_or_default());
    let trimmed = env_key.trim();
    if trimmed.is_empty() {
        return Err(
            "no agent37 API key: set AGENT37_API_KEY or config.keyFile to a 0600 file".to_owned(),
        );
    }
    Ok(ApiKey(Zeroizing::new(trimmed.to_owned())))
}

/// Reads the key from a regular, owner-only, absolute-path file.
fn read_key_file(path: &Path) -> Result<ApiKey, String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("key file {shown} must be an absolute path"));
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat key file {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("key file {shown} is not a regular file"));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "key file {shown} has mode {mode:04o}; it must be 0600 (chmod 600 {shown})"
        ));
    }
    let contents = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|e| format!("cannot read key file {shown}: {e}"))?,
    );
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Err(format!("key file {shown} is empty"));
    }
    Ok(ApiKey(Zeroizing::new(trimmed.to_owned())))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test setup failures should abort loudly"
)]
mod tests {
    //! Key resolution behaviour.

    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// Stand-in key that must never show up in errors or Debug output.
    const SECRET: &str = "sk-test-secret-value";

    /// Writes `body` to a file with `mode`; the directory lives as long as the guard.
    fn key_file(body: &str, mode: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    /// Config pointing at `path`.
    fn cfg(path: &Path) -> serde_json::Value {
        json!({ "keyFile": path.to_str().unwrap() })
    }

    /// A 0600 file is accepted and whitespace is trimmed.
    #[test]
    fn key_file_0600_is_accepted_and_trimmed() {
        let (_d, p) = key_file(&format!("  {SECRET}\n"), 0o600);
        let key = resolve(&cfg(&p), None).unwrap();
        assert_eq!(key.expose(), SECRET);
    }

    /// Read-only owner files are fine.
    #[test]
    fn key_file_0400_is_accepted() {
        let (_d, p) = key_file(SECRET, 0o400);
        assert_eq!(resolve(&cfg(&p), None).unwrap().expose(), SECRET);
    }

    /// World-readable files are refused and the contents stay out of the error.
    #[test]
    fn key_file_with_group_or_world_bits_is_refused() {
        let (_d, p) = key_file(SECRET, 0o644);
        let err = resolve(&cfg(&p), None).unwrap_err();
        assert!(err.contains("0644"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
        assert!(!err.contains(SECRET), "{err}");
    }

    /// Group-readable files are refused too.
    #[test]
    fn key_file_0640_is_refused() {
        let (_d, p) = key_file(SECRET, 0o640);
        let err = resolve(&cfg(&p), None).unwrap_err();
        assert!(err.contains("0640"), "{err}");
    }

    /// The file takes priority over the environment.
    #[test]
    fn key_file_wins_over_env() {
        let (_d, p) = key_file(SECRET, 0o600);
        let key = resolve(&cfg(&p), Some("from-env".to_owned())).unwrap();
        assert_eq!(key.expose(), SECRET);
    }

    /// Without a file the environment value is used, trimmed.
    #[test]
    fn env_used_when_no_key_file() {
        let key = resolve(&json!({}), Some(" from-env\n".to_owned())).unwrap();
        assert_eq!(key.expose(), "from-env");
    }

    /// No source at all is an error, as is a blank env value.
    #[test]
    fn missing_key_is_an_error() {
        assert!(resolve(&json!({}), None).is_err());
        assert!(resolve(&json!({}), Some("  ".to_owned())).is_err());
    }

    /// Relative paths depend on the working directory, so they are refused.
    #[test]
    fn relative_key_file_is_refused() {
        let err = resolve(&json!({ "keyFile": "key.txt" }), None).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    /// Only regular files are read.
    #[test]
    fn directory_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(resolve(&cfg(dir.path()), None).is_err());
    }

    /// A whitespace-only file holds no key.
    #[test]
    fn empty_key_file_is_refused() {
        let (_d, p) = key_file(" \n", 0o600);
        assert!(resolve(&cfg(&p), None).unwrap_err().contains("empty"));
    }

    /// A key in config is rejected and not repeated back.
    #[test]
    fn config_with_api_key_is_refused_without_echoing_it() {
        for field in ["apiKey", "api_key"] {
            let err = resolve(&json!({ field: SECRET }), Some("x".to_owned())).unwrap_err();
            assert!(err.contains("must not carry the API key"), "{err}");
            assert!(!err.contains(SECRET), "{err}");
        }
    }

    /// Debug output is redacted.
    #[test]
    fn debug_does_not_show_the_key() {
        let key = resolve(&json!({}), Some(SECRET.to_owned())).unwrap();
        let shown = format!("{key:?}");
        assert!(!shown.contains(SECRET), "{shown}");
        assert_eq!(shown, "ApiKey(<redacted>)");
    }
}
