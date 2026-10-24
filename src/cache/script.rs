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
/// agent37's init bind-mounts `/root/docker` onto itself at boot, and through a symlink it
/// binds the store directory onto itself, so Docker's data lands in the store. A path that is
/// a mount point is unmounted first. An existing directory moves into an absent store
/// directory, so a running daemon keeps its open files until the reboot.
///
/// Prints `restored NAME` for a volume whose directory came with the seed and `empty NAME`
/// otherwise, then `reboot` when it replaced a directory a running daemon holds.
pub(super) fn acquire(plan: &Plan) -> String {
    let mut s = prologue("acquire");
    s.push_str(r#"re=0; mkdir -p "$C"; chmod 700 "$C"; "#);
    let _ = write!(
        s,
        r#"link() {{ if [ -L "$2" ]; then [ "$(readlink "$2")" = "$1" ] && return 0; rm -f "$2"; elif [ -e "$2" ]; then case "$2" in {DAEMON_PATHS}) if pidof dockerd containerd podman >/dev/null 2>&1; then re=1; fi;; esac; if mountpoint -q "$2"; then umount -l "$2"; fi; if [ -e "$1" ]; then rm -rf "$2"; else mkdir -p "$(dirname "$1")"; mv "$2" "$1"; fi; fi; mkdir -p "$1" "$(dirname "$2")"; ln -s "$1" "$2"; }}; "#
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
/// any of its dirs, then strips `/root` down to the store, `.ssh`, agent37's `.agent37` and
/// the links into the store. A kept `/root/docker` link lets the seed boot straight onto its
/// data. A mount point, such as the `/root/docker` agent37's init binds when no volume
/// replaced it, cannot be removed, so it is emptied without crossing into other mounts.
///
/// agent37's stop halts the VM without signalling its processes, and dockerd cannot be
/// stopped first because its exit ends the VM's entrypoint. So the script freezes `/root`,
/// which flushes it and holds every later write, leaving the disk consistent for the halt.
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
        r#"find "$P{HOME}" -mindepth 1 -maxdepth 1 ! -name {STORE_DIR} ! -name .ssh ! -name .agent37 ! -lname "$C/*" -exec sh -c 'for e; do if mountpoint -q "$e"; then find "$e" -mindepth 1 -xdev -delete; else rm -rf -- "$e"; fi; done' strip {{}} +; sync; fsfreeze -f "$P{HOME}""#
    );
    Ok(s)
}

/// File on `/run`, a tmpfs a stop clears, whose link target is the lease that claimed the Box.
const CLAIM: &str = "/run/crabbox-claim";

/// Claims the running Box for `lease` unless another lease did since it started; prints the
/// lease that holds the claim. `ln -s` fails when the link exists, so one lease wins.
pub(super) fn claim(lease: &str) -> String {
    format!(
        r#"{}mkdir -p "$P/run"; ln -s {} "$P{CLAIM}" 2>/dev/null || :; readlink "$P{CLAIM}""#,
        prologue("claim"),
        q(lease)
    )
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
