//! Drives the adapter binary against a fake agent37 for the cache tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::fake_agent37::FakeAgent37;

/// Key the fake accepts.
pub(crate) const KEY: &str = "sk_test_fake";
/// Data dir of the npm volume, relative to the store.
pub(crate) const NPM: &str = "g1/npm/npm-82ec3afe";
/// Data dir of the docker volume's `~/docker` path, relative to the store.
pub(crate) const DOCKER: &str = "g1x/docker/docker-784af2d5";
/// Cache scope the default acquire sends.
pub(crate) const SCOPE: &str = "0123456789abcdef";
/// Another cache scope.
pub(crate) const OTHER_SCOPE: &str = "fedcba9876543210";

/// Runs the binary with `request`; returns the exit code and parsed stdout.
pub(crate) fn run(request: &Value) -> (i32, Value) {
    run_in(request, &std::env::temp_dir())
}

/// Runs the binary with `request` and `tmp` as its temp dir, which holds the host's claim
/// lock; a separate `tmp` stands for another host.
pub(crate) fn run_in(request: &Value, tmp: &Path) -> (i32, Value) {
    let (code, out, _) = run_with_diag(request, tmp);
    (code, out)
}

/// [`run_in`], also returning stderr.
pub(crate) fn run_with_diag(request: &Value, tmp: &Path) -> (i32, Value, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"))
        .env("AGENT37_API_KEY", KEY)
        .env("TMPDIR", tmp)
        .env_remove("AGENT37_KEY_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        request.to_string().as_bytes(),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let raw = String::from_utf8(out.stdout).unwrap();
    let parsed = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad stdout {raw:?}: {e}"));
    let diag = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.code().unwrap(), parsed, diag)
}

/// Config for owner `me` against `fake`, with extra keys merged in.
pub(crate) fn config(fake: &FakeAgent37, extra: &Value) -> Value {
    let mut c = json!({ "apiUrl": fake.url(), "owner": "me", "readyTimeoutSeconds": 5 });
    if let (Some(c), Some(extra)) = (c.as_object_mut(), extra.as_object()) {
        c.extend(extra.clone());
    }
    c
}

/// npm in group `g1`, and the sealed docker volume in its own group `g1x`.
pub(crate) fn volumes() -> Value {
    json!([
        {"group": "g1", "name": "npm", "key": "g1-npm", "paths": ["~/.npm"]},
        {"group": "g1x", "name": "docker", "key": "g1x-docker",
         "paths": ["~/docker", "/var/lib/containerd"], "sealed": true},
    ])
}

/// Acquires lease `n` of class `class`; returns the lease object.
pub(crate) fn acquire(fake: &FakeAgent37, n: u32, class: &str) -> Value {
    acquire_as(fake, "me", n, class, &std::env::temp_dir())
}

/// Acquires lease `n` of class `class` for `owner` on the host whose temp dir is `tmp`.
pub(crate) fn acquire_as(
    fake: &FakeAgent37,
    owner: &str,
    n: u32,
    class: &str,
    tmp: &Path,
) -> Value {
    let extra = json!({ "cacheScope": SCOPE });
    acquire_with(fake, (owner, n, class, tmp), &extra).0
}

/// Acquires lease `n` with `extra` merged into the desired lease (`cacheVolumes` defaults
/// to [`volumes`]); returns the lease and stderr.
pub(crate) fn acquire_with(
    fake: &FakeAgent37,
    (owner, n, class, tmp): (&str, u32, &str, &Path),
    extra: &Value,
) -> (Value, String) {
    let config = config(fake, &json!({ "owner": owner }));
    let mut desired = json!({"leaseId": format!("cbx_{n}"), "slug": format!("s{n}"),
                             "name": format!("box-{n}"), "class": class, "cacheVolumes": volumes()});
    if let (Some(d), Some(extra)) = (desired.as_object_mut(), extra.as_object()) {
        d.extend(extra.clone());
    }
    let (code, out, diag) = run_with_diag(
        &json!({"protocolVersion": 1, "operation": "acquire", "config": config, "desired": desired}),
        tmp,
    );
    assert_eq!(code, 0, "acquire failed: {out} {diag}");
    (out["lease"].clone(), diag)
}

/// Acquires lease `n` of class `standard` for `me` with `extra` merged into the desired lease.
pub(crate) fn acquire_extra(fake: &FakeAgent37, n: u32, extra: &Value) -> (Value, String) {
    acquire_with(fake, ("me", n, "standard", &std::env::temp_dir()), extra)
}

/// Releases `lease`; returns the exit code.
pub(crate) fn release(fake: &FakeAgent37, lease: &Value) -> i32 {
    release_as(fake, "me", lease)
}

/// Releases `lease` for `owner`; returns the exit code.
pub(crate) fn release_as(fake: &FakeAgent37, owner: &str, lease: &Value) -> i32 {
    let request = json!({
        "protocolVersion": 1, "operation": "release",
        "config": config(fake, &json!({ "owner": owner })),
        "lease": {"leaseId": lease["leaseId"], "slug": lease["slug"],
                  "name": lease["name"], "cloudId": lease["cloudId"]},
    });
    run(&request).0
}

/// `(name, outcome)` of each echoed volume.
pub(crate) fn outcomes(lease: &Value) -> Vec<(String, String)> {
    lease["cacheVolumes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().into(),
                e["outcome"].as_str().unwrap().into(),
            )
        })
        .collect()
}

/// Every volume's outcome is `want`.
pub(crate) fn all(want: &str) -> Vec<(String, String)> {
    vec![("npm".into(), want.into()), ("docker".into(), want.into())]
}

/// Store directory `rel` on `root`.
pub(crate) fn store(root: &Path, rel: &str) -> PathBuf {
    root.join("root/.crabbox-cache").join(rel)
}

/// The instance `id` as the fake holds it.
pub(crate) fn instance(fake: &FakeAgent37, id: &Value) -> Value {
    fake.instance(id.as_str().unwrap()).unwrap_or(Value::Null)
}

/// Entries of `dir`, sorted.
pub(crate) fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Marks the docker volume sealed on `root`, as moor's seal script does.
pub(crate) fn seal_docker(root: &Path) {
    std::fs::write(store(root, DOCKER).join(".crabbox-sealed"), "").unwrap();
    std::fs::write(
        store(
            root,
            "g1x/docker/var-lib-containerd-399dacdf/.crabbox-sealed",
        ),
        "",
    )
    .unwrap();
}

/// Requests for instance `id` with `method` and path `suffix` from request `from` on.
pub(crate) fn calls(
    fake: &FakeAgent37,
    from: usize,
    method: &str,
    id: &Value,
    suffix: &str,
) -> usize {
    let path = format!("/v1/instances/{}{suffix}", id.as_str().unwrap());
    fake.requests()
        .iter()
        .skip(from)
        .filter(|r| r.method == method && r.path == path)
        .count()
}
