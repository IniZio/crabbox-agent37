//! The shell scripts run on the VM at acquire and at release.
//!
//! Every path starts with `$P`, which is empty on a VM; the fake API sets it to a scratch
//! directory so tests run the same text on the host.

use std::fmt::Write as _;

use super::{Plan, HOME, SEAL_MARKER, STORE_DIR};

/// Paths a running daemon holds open; replacing one needs a reboot.
const DAEMON_PATHS: &str = r#""$P/root/docker"|"$P/var/lib/containerd"|"$P/var/lib/containers""#;

/// POSIX single-quoted `s`.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Prologue shared by both scripts: `$C` is the store.
fn prologue(tag: &str) -> String {
    format!(": crabbox-cache-{tag}; set -eu; P=; C=\"$P{HOME}/{STORE_DIR}\"; ")
}

/// Sets up every planned volume on a fresh or restored Box.
///
/// A path that is a mount point (agent37's init bind-mounts `/root/docker`) is unmounted
/// first; on later boots the init mounts through the symlink.
///
/// Prints `restored NAME` for a volume whose directory came with the seed and `empty NAME`
/// otherwise, then `reboot` when it replaced a directory a running daemon holds.
pub(super) fn acquire(plan: &Plan) -> String {
    let mut s = prologue("acquire");
    s.push_str(r#"re=0; mkdir -p "$C"; chmod 700 "$C"; "#);
    let _ = write!(
        s,
        r#"link() {{ mkdir -p "$1"; if [ -L "$2" ]; then [ "$(readlink "$2")" = "$1" ] && return 0; rm -f "$2"; elif [ -e "$2" ]; then case "$2" in {DAEMON_PATHS}) if pidof dockerd containerd podman >/dev/null 2>&1; then re=1; fi;; esac; if mountpoint -q "$2"; then umount -l "$2"; fi; rm -rf "$2"; fi; mkdir -p "$(dirname "$2")"; ln -s "$1" "$2"; }}; "#
    );
    s.push_str(
        r#"vol() { if [ -d "$C/$1" ]; then echo "restored $2"; else echo "empty $2"; fi; }; "#,
    );
    let mut docker = false;
    for g in &plan.groups {
        for v in &g.volumes {
            let name = v.name.as_str();
            let _ = write!(
                s,
                "vol {} {}; ",
                q(&format!("{}/{name}", g.id.as_str())),
                q(name)
            );
            for (path, dir) in g.dirs(v) {
                docker |= path == "/root/docker";
                let _ = write!(s, r#"link "$C/"{} "$P"{}; "#, q(&dir), q(path));
                if v.sealed {
                    let _ = write!(s, r#"rm -f "$C/"{}/{SEAL_MARKER}; "#, q(&dir));
                }
            }
        }
    }
    if docker {
        let _ = write!(
            s,
            r#"mkdir -p "$P/etc/docker"; python3 -c {} "$P/etc/docker/daemon.json"; "#,
            q(crate::docker_root::MERGE)
        );
    }
    s.push_str(r#"if [ "$re" = 1 ]; then echo reboot; fi"#);
    s
}

/// `(name, data dirs relative to the store)` of every sealed volume, as JSON for metadata.
pub(super) fn sealed(plan: &Plan) -> String {
    let rows: Vec<Vec<String>> = plan
        .groups
        .iter()
        .flat_map(|g| g.volumes.iter().filter(|v| v.sealed).map(move |v| (g, v)))
        .map(|(g, v)| {
            std::iter::once(v.name.as_str().to_owned())
                .chain(g.dirs(v).into_iter().map(|(_, d)| d))
                .collect()
        })
        .collect();
    serde_json::Value::from(rows).to_string()
}

/// Readies a released Box to become a seed: empties each sealed volume missing a marker in
/// any of its dirs, then strips `/root` down to the store, `.ssh` and the links into the
/// store, then syncs. A kept `/root/docker` link lets the seed boot straight onto its data.
///
/// `sealed` is what [`sealed`] produced; an unreadable value refuses, so the Box is deleted.
pub(super) fn release(sealed: &str) -> Result<String, String> {
    let rows: Vec<Vec<String>> =
        serde_json::from_str(sealed).map_err(|e| format!("sealed volume list: {e}"))?;
    let mut s = prologue("release");
    let _ = write!(
        s,
        r#"seal() {{ n=$1; shift; for d in "$@"; do if [ ! -e "$C/$d/{SEAL_MARKER}" ]; then for e in "$@"; do mkdir -p "$C/$e"; find "$C/$e" -mindepth 1 -maxdepth 1 -exec rm -rf -- {{}} +; done; echo "emptied $n"; return 0; fi; done; }}; "#
    );
    for row in rows {
        let words: Vec<String> = row.iter().map(|w| q(w)).collect();
        let _ = write!(s, "seal {}; ", words.join(" "));
    }
    let _ = write!(
        s,
        r#"find "$P{HOME}" -mindepth 1 -maxdepth 1 ! -name {STORE_DIR} ! -name .ssh ! -lname "$C/*" -exec rm -rf -- {{}} +; sync"#
    );
    Ok(s)
}

/// What the acquire script reported: restored names and whether a reboot is due.
pub(super) fn parse_acquire(stdout: &str) -> (Vec<String>, bool) {
    let mut restored = Vec::new();
    let mut reboot = false;
    for line in stdout.lines() {
        match line.split_once(' ') {
            Some(("restored", name)) => restored.push(name.to_owned()),
            _ if line == "reboot" => reboot = true,
            _ => {}
        }
    }
    (restored, reboot)
}
