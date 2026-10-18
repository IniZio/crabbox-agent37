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
}

/// Reads and validates `owner`, `region`, `readyTimeoutSeconds`, `keyFile`, `template` and `diskGb`.
pub(crate) fn settings(config: &Value) -> Result<Settings, String> {
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
    let template = match config.get("template") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if !t.trim().is_empty() => Some(t.clone()),
        Some(_) => return Err("config.template must be a non-empty template name".to_owned()),
    };
    let disk_gb = match config.get("diskGb") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("config.diskGb must be a whole number of GB")?,
        ),
    };
    Ok(Settings {
        owner: owner.to_owned(),
        region: region.to_owned(),
        timeout,
        key_file: key_file.filter(|k| !k.is_empty()).map(str::to_owned),
        template,
        disk_gb,
    })
}

/// True for a non-empty run of `[a-z0-9]`.
pub(crate) fn label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Rejects a disk size outside agent37's range for the shape (2 GB up to 12/20/40/80 GB).
pub(crate) fn check_disk(disk_gb: u32, cpu: u32, memory_gb: u32) -> Result<(), String> {
    let max = match cpu {
        2 => 12,
        4 => 20,
        8 => 40,
        _ => 80,
    };
    if (2..=max).contains(&disk_gb) {
        return Ok(());
    }
    Err(format!(
        "config.diskGb {disk_gb} is outside 2-{max} GB for a {cpu} vCPU / {memory_gb} GB VM"
    ))
}
