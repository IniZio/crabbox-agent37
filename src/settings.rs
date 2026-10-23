//! Adapter settings read from the request config.

use serde_json::Value;

/// Validated adapter settings from the request config.
pub(crate) struct Settings {
    /// Workspace-unique owner label embedded in instance names.
    pub(crate) owner: String,
    /// `us` or `eu`.
    pub(crate) region: String,
    /// Seconds to wait for a running VM with a readable host key.
    pub(crate) timeout: u64,
    /// Key file the tunnel reads the API key from, as configured.
    pub(crate) key_file: Option<String>,
    /// Template name to create VMs from, when configured.
    pub(crate) template: Option<String>,
    /// Disk size in GB, when configured.
    pub(crate) disk_gb: Option<u32>,
    /// Where `disk_gb` came from, for error messages.
    pub(crate) disk_source: &'static str,
}

/// Env var that overrides `config.template`.
const TEMPLATE_ENV: &str = "CRABBOX_AGENT37_TEMPLATE";
/// Env var that overrides `config.diskGb`.
const DISK_ENV: &str = "CRABBOX_AGENT37_DISK_GB";

/// Reads and validates `owner`, `region`, `readyTimeoutSeconds`, `keyFile`, `template` and `diskGb`.
///
/// `CRABBOX_AGENT37_TEMPLATE` and `CRABBOX_AGENT37_DISK_GB` in the process env win over the config.
pub(crate) fn settings(config: &Value) -> Result<Settings, String> {
    settings_with_env(config, |k| std::env::var(k).ok())
}

/// [`settings`] with the env lookup injected.
pub(crate) fn settings_with_env(
    config: &Value,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Settings, String> {
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    let owner = config
        .get("owner")
        .and_then(Value::as_str)
        .filter(|o| o.len() <= 32 && o.split('-').all(label_ok))
        .ok_or("config.owner is required: lowercase letters, digits and single dashes, at most 32 characters")?;
    let region = match config.get("region") {
        None | Some(Value::Null) => "us",
        Some(Value::String(r)) if r == "us" || r == "eu" => r.as_str(),
        Some(_) => return Err("config.region must be us or eu".to_owned()),
    };
    let timeout = match config.get("readyTimeoutSeconds") {
        None | Some(Value::Null) => 300,
        Some(v) => v
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("config.readyTimeoutSeconds must be a positive integer")?,
    };
    let key_file = config.get("keyFile").and_then(Value::as_str);
    let template = match (env(TEMPLATE_ENV), config.get("template")) {
        (Some(t), _) if !t.trim().is_empty() => Some(t.trim().to_owned()),
        (Some(_), _) => return Err(format!("{TEMPLATE_ENV} must be a non-empty template name")),
        (None, None | Some(Value::Null)) => None,
        (None, Some(Value::String(t))) if !t.trim().is_empty() => Some(t.trim().to_owned()),
        (None, Some(_)) => {
            return Err("config.template must be a non-empty template name".to_owned())
        }
    };
    let (disk_gb, disk_source) = match (env(DISK_ENV), config.get("diskGb")) {
        (Some(d), _) => (
            Some(
                d.trim()
                    .parse::<u32>()
                    .map_err(|_| format!("{DISK_ENV} must be a whole number of GB"))?,
            ),
            DISK_ENV,
        ),
        (None, None | Some(Value::Null)) => (None, "config.diskGb"),
        (None, Some(v)) => (
            Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or("config.diskGb must be a whole number of GB")?,
            ),
            "config.diskGb",
        ),
    };
    Ok(Settings {
        owner: owner.to_owned(),
        region: region.to_owned(),
        timeout,
        key_file: key_file.filter(|k| !k.is_empty()).map(str::to_owned),
        template,
        disk_gb,
        disk_source,
    })
}

/// True for a non-empty run of `[a-z0-9]`.
pub(crate) fn label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Disk size in GB to create a VM with: `disk_gb` when set, else the shape's maximum.
///
/// The maximum is the default because growing a disk later needs a resize, and agent37
/// cold-boots a VM on every resize, which kills whatever the Box is running.
/// Rejects a size outside agent37's range for the shape (2 GB up to 12/20/40/80 GB).
pub(crate) fn disk_for(
    source: &str,
    disk_gb: Option<u32>,
    cpu: u32,
    memory_gb: u32,
) -> Result<u32, String> {
    let max = match cpu {
        2 => 12,
        4 => 20,
        8 => 40,
        _ => 80,
    };
    match disk_gb {
        None => Ok(max),
        Some(gb) if (2..=max).contains(&gb) => Ok(gb),
        Some(gb) => Err(format!(
            "{source} {gb} is outside 2-{max} GB for a {cpu} vCPU / {memory_gb} GB VM"
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn with(config: &Value, vars: &[(&str, &str)]) -> Result<Settings, String> {
        settings_with_env(config, |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    #[test]
    fn env_beats_config_beats_default() {
        let base = json!({"owner": "alice"});
        let s = with(&base, &[]).unwrap();
        assert_eq!((s.template, s.disk_gb), (None, None));
        let cfg = json!({"owner": "alice", "template": "a", "diskGb": 4});
        let s = with(&cfg, &[]).unwrap();
        assert_eq!((s.template.as_deref(), s.disk_gb), (Some("a"), Some(4)));
        let s = with(&cfg, &[(TEMPLATE_ENV, "b"), (DISK_ENV, "6")]).unwrap();
        assert_eq!((s.template.as_deref(), s.disk_gb), (Some("b"), Some(6)));
        assert_eq!(s.disk_source, DISK_ENV);
    }

    #[test]
    fn empty_env_is_unset_and_bad_env_errors() {
        let cfg = json!({"owner": "alice", "template": "a", "diskGb": 4});
        let s = with(&cfg, &[(TEMPLATE_ENV, ""), (DISK_ENV, "")]).unwrap();
        assert_eq!((s.template.as_deref(), s.disk_gb), (Some("a"), Some(4)));
        let err = with(&cfg, &[(DISK_ENV, "x")]).err().unwrap();
        assert!(err.starts_with(DISK_ENV), "{err}");
        let err = with(&cfg, &[(TEMPLATE_ENV, "  ")]).err().unwrap();
        assert!(err.starts_with(TEMPLATE_ENV), "{err}");
    }
}
