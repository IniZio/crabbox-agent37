//! Cache volumes on agent37: the released Box itself becomes the seed of the next one.
//!
//! A group's data lives on the billed disk at
//! `/root/.crabbox-cache/<group>/<name>/<slug>-<hash>`, and every declared path is a symlink to
//! it: a VM has no systemd, ignores fstab and loses bind mounts on reboot, while symlinks
//! survive a stop and start.

mod gc;
mod script;
mod seed;

use std::fmt::Write as _;

use ring::digest;

use crate::protocol::{CacheVolumeEcho, CacheVolumeRequest};

pub(crate) use gc::{delete_seeds, gc};
pub(crate) use seed::{claim, is_lease, is_seed, release, setup, take, Taken};

/// The login user's home; `~/` paths resolve here.
const HOME: &str = "/root";
/// Directory under [`HOME`] that holds every group's data; a strip keeps it.
const STORE_DIR: &str = ".crabbox-cache";
/// File the guest leaves in each path of a sealed volume once it sealed it.
const SEAL_MARKER: &str = ".crabbox-sealed";
/// Longest group, name or key.
const MAX_TOKEN: usize = 96;
/// Characters allowed in a path component besides ASCII alphanumerics.
const PATH_PUNCT: &str = "._@+-";
/// Absolute roots a volume may sit strictly below.
const BELOW_ROOTS: [&str; 2] = ["/opt", "/var/cache"];
/// Absolute roots a volume may be, or sit below.
const AT_ROOTS: [&str; 2] = ["/var/lib/containerd", "/var/lib/containers"];
/// Home directories never cached: credentials, and the store itself.
const HOME_REFUSED: [&str; 2] = [".ssh", STORE_DIR];

/// A group id, volume name or key: `[A-Za-z0-9._-]{1,96}`, never `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Token(String);

impl Token {
    /// Validates `s`; the only way a request string reaches a file name.
    fn parse(s: &str) -> Result<Self, String> {
        let charset = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if s.is_empty() || s.len() > MAX_TOKEN || !charset || s == "." || s == ".." {
            return Err("must be 1 to 96 characters of [A-Za-z0-9._-]".to_owned());
        }
        Ok(Self(s.to_owned()))
    }

    /// The token text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A guest path, resolved against [`HOME`] when given as `~/<rel>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestPath {
    /// Absolute path in the guest.
    abs: String,
    /// The path as the request wrote it; its data dir name derives from this.
    written: String,
}

impl GuestPath {
    /// Parses `~/<rel>` (not `~/.ssh` or the store), `/opt/<rel>`, `/var/cache/<rel>`, or
    /// `/var/lib/containerd` and `/var/lib/containers` and below. Components are
    /// `[A-Za-z0-9._@+-]`, never `.` or `..`.
    fn parse(s: &str) -> Result<Self, String> {
        let (home, rest) = match s.strip_prefix("~/") {
            Some(rel) => (true, rel),
            None => match s.strip_prefix('/') {
                Some(rel) => (false, rel),
                None => return Err("must start with ~/ or /".to_owned()),
            },
        };
        let component_ok = |c: &str| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || PATH_PUNCT.contains(ch))
        };
        if !rest.split('/').all(component_ok) {
            return Err("has an empty, dot or unsupported component".to_owned());
        }
        let below = |root: &str, p: &str| {
            p.strip_prefix(root)
                .is_some_and(|r| r.len() > 1 && r.starts_with('/'))
        };
        let allowed = if home {
            !HOME_REFUSED.iter().any(|r| rest == *r || below(r, rest))
        } else {
            BELOW_ROOTS.iter().any(|r| below(r, s))
                || AT_ROOTS.iter().any(|r| s == *r || below(r, s))
        };
        if !allowed {
            return Err(
                "must be under ~/ (not ~/.ssh), /opt/, /var/cache/, /var/lib/containerd or /var/lib/containers"
                    .to_owned(),
            );
        }
        let abs = if home {
            format!("{HOME}/{rest}")
        } else {
            s.to_owned()
        };
        Ok(Self {
            abs,
            written: s.to_owned(),
        })
    }
}

/// One declared volume of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Volume {
    /// Directory name inside the group.
    name: Token,
    /// Absolute guest paths symlinked to it, at least one.
    paths: Vec<GuestPath>,
    /// The guest seals it at close and leaves a marker in each path.
    sealed: bool,
}

/// One generation chain and the volumes inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    /// Group id.
    id: Token,
    /// Volumes in request order.
    volumes: Vec<Volume>,
}

impl Group {
    /// `(path, its data dir relative to the store)` for every path of `volume`.
    fn dirs<'a>(&self, volume: &'a Volume) -> Vec<(&'a str, String)> {
        volume
            .paths
            .iter()
            .map(|p| {
                let dir = format!(
                    "{}/{}/{}",
                    self.id.as_str(),
                    volume.name.as_str(),
                    data_dir_name(&p.written)
                );
                (p.abs.as_str(), dir)
            })
            .collect()
    }
}

/// Data dir name of a guest path as written: a readable, never hidden slug plus 8 hex chars
/// of its SHA-256. Tools resolve symlinks, so the name must be a plain path segment, and two
/// paths whose slugs agree (`/opt/a-b`, `/opt/a/b`) still get different names.
fn data_dir_name(written: &str) -> String {
    let slug: String = written
        .chars()
        .filter_map(|c| match c {
            '/' => Some('-'),
            c if c.is_ascii_alphanumeric() || "._-".contains(c) => Some(c),
            _ => None,
        })
        .collect();
    let slug = slug.trim_start_matches(['-', '.']).trim_end_matches('-');
    format!("{slug}-{}", sha256_hex(written, 4))
}

/// Lowercase hex of the first `bytes` bytes of the SHA-256 of `text`.
fn sha256_hex(text: &str, bytes: usize) -> String {
    digest::digest(&digest::SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .take(bytes)
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The groups a lease gets, plus the requested names that were refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Valid groups in first-seen order.
    groups: Vec<Group>,
    /// `(name as sent, reason)` for refused volumes.
    skipped: Vec<(String, String)>,
}

/// The VM a seed was made on; a seed only serves a Box of the same shape.
pub(crate) struct Shape<'a> {
    /// vCPU count.
    pub(crate) cpu: u32,
    /// Memory in GB.
    pub(crate) memory_gb: u32,
    /// Disk in GB.
    pub(crate) disk_gb: u32,
    /// Template name, `None` for the adapter default.
    pub(crate) template: Option<&'a str>,
    /// Region the VM runs in.
    pub(crate) region: &'a str,
}

/// `desired.cacheScope` when it is 16 lowercase hex; anything else sent is reported and
/// dropped, so the Box goes cold and is never seeded.
pub(crate) fn scope<'a>(sent: Option<&'a str>, diag: &mut dyn std::io::Write) -> Option<&'a str> {
    let sent = sent?;
    let valid = sent.len() == 16 && sent.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        let _ = writeln!(
            diag,
            "acquire: ignoring desired.cacheScope {sent:?}: not 16 lowercase hex; no seed"
        );
    }
    valid.then_some(sent)
}

/// Seed key, as 16 hex: the cache scope, owner, template, VM shape and region. A seed holds
/// the whole Box, so only a Box of the same scope may reuse it; groups are not part of it.
pub(crate) fn seed_key(scope: &str, owner: &str, shape: &Shape<'_>) -> String {
    let text = format!(
        "{scope}\n{owner}\n{}\n{}x{}x{}\n{}",
        shape.template.unwrap_or(""),
        shape.cpu,
        shape.memory_gb,
        shape.disk_gb,
        shape.region
    );
    sha256_hex(&text, 8)
}

/// A request entry as a typed volume, or why it is refused.
fn parse_entry(req: &CacheVolumeRequest) -> Result<(Token, Volume), String> {
    let group = Token::parse(&req.group).map_err(|e| format!("group {e}"))?;
    let name = Token::parse(&req.name).map_err(|e| format!("name {e}"))?;
    Token::parse(&req.key).map_err(|e| format!("key {e}"))?;
    if req.paths.is_empty() {
        return Err("no paths".to_owned());
    }
    let paths = req
        .paths
        .iter()
        .map(|p| GuestPath::parse(p).map_err(|e| format!("path {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        group,
        Volume {
            name,
            paths,
            sealed: req.sealed,
        },
    ))
}

impl Plan {
    /// Groups the request's volumes; a bad or duplicate entry is skipped, never an error.
    pub(crate) fn new(requests: &[CacheVolumeRequest]) -> Self {
        let mut out = Self::default();
        let mut seen_paths: Vec<GuestPath> = Vec::new();
        for req in requests {
            let (group, volume) = match parse_entry(req) {
                Ok(entry) => entry,
                Err(reason) => {
                    out.skipped.push((req.name.clone(), reason));
                    continue;
                }
            };
            let index = out.groups.iter().position(|g| g.id == group);
            if index
                .and_then(|i| out.groups.get(i))
                .is_some_and(|g| g.volumes.iter().any(|v| v.name == volume.name))
            {
                out.skipped
                    .push((req.name.clone(), "duplicate name in group".to_owned()));
                continue;
            }
            if volume.paths.iter().any(|p| seen_paths.contains(p)) {
                out.skipped
                    .push((req.name.clone(), "path already cached".to_owned()));
                continue;
            }
            seen_paths.extend(volume.paths.iter().cloned());
            match index.and_then(|i| out.groups.get_mut(i)) {
                Some(g) => g.volumes.push(volume),
                None => out.groups.push(Group {
                    id: group,
                    volumes: vec![volume],
                }),
            }
        }
        out
    }

    /// True when no group is valid, so the Box runs without a cache.
    pub(crate) const fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Echo of every requested name: `restored` names came from seed generation `generation`.
    pub(crate) fn echo(
        &self,
        restored: &[String],
        generation: Option<&str>,
    ) -> Vec<CacheVolumeEcho> {
        let names = self.groups.iter().flat_map(|g| &g.volumes).map(|v| {
            let name = v.name.as_str().to_owned();
            let (outcome, detail) = match generation {
                Some(gen) if restored.contains(&name) => ("restored", format!("gen {gen}")),
                _ => ("empty", "no generation yet".to_owned()),
            };
            (name, outcome, detail)
        });
        let refused = self
            .skipped
            .iter()
            .map(|(name, why)| (name.clone(), "skipped", why.clone()));
        names
            .chain(refused)
            .map(|(name, outcome, detail)| CacheVolumeEcho {
                name,
                outcome: outcome.to_owned(),
                detail,
            })
            .collect()
    }

    /// Every requested name as `skipped` with `reason`, for a Box whose cache setup failed.
    pub(crate) fn all_skipped(&self, reason: &str) -> Vec<CacheVolumeEcho> {
        let mut out = self.echo(&[], None);
        for e in &mut out {
            if e.outcome != "skipped" {
                "skipped".clone_into(&mut e.outcome);
                reason.clone_into(&mut e.detail);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
