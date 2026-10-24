//! Cache volumes on agent37: a released Box becomes the seed of the next acquire of its key.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_agent37.rs"]
mod fake_agent37;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use fake_agent37::FakeAgent37;
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";
/// Data dir of the npm volume, relative to the store.
const NPM: &str = "g1/npm/root-.npm";
/// Data dir of the docker volume's `~/docker` path, relative to the store.
const DOCKER: &str = "g1x/docker/root-docker";

/// Runs the binary with `request`; returns the exit code and parsed stdout.
fn run(request: &Value) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"))
        .env("AGENT37_API_KEY", KEY)
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
    (out.status.code().unwrap(), parsed)
}

/// Config for owner `me` against `fake`, with extra keys merged in.
fn config(fake: &FakeAgent37, extra: &Value) -> Value {
    let mut c = json!({ "apiUrl": fake.url(), "owner": "me", "readyTimeoutSeconds": 5 });
    if let (Some(c), Some(extra)) = (c.as_object_mut(), extra.as_object()) {
        c.extend(extra.clone());
    }
    c
}

/// npm in group `g1`, and the sealed docker volume in its own group `g1x`.
fn volumes() -> Value {
    json!([
        {"group": "g1", "name": "npm", "key": "g1-npm", "paths": ["~/.npm"]},
        {"group": "g1x", "name": "docker", "key": "g1x-docker",
         "paths": ["~/docker", "/var/lib/containerd"], "sealed": true},
    ])
}

/// Acquires lease `n` of class `class`; returns the lease object.
fn acquire(fake: &FakeAgent37, n: u32, class: &str) -> Value {
    let (code, out) = run(&json!({
        "protocolVersion": 1, "operation": "acquire", "config": config(fake, &json!({})),
        "desired": {"leaseId": format!("cbx_{n}"), "slug": format!("s{n}"),
                    "name": format!("box-{n}"), "class": class, "cacheVolumes": volumes()},
    }));
    assert_eq!(code, 0, "acquire failed: {out}");
    out["lease"].clone()
}

/// Releases `lease`; returns the exit code.
fn release(fake: &FakeAgent37, lease: &Value) -> i32 {
    let request = json!({
        "protocolVersion": 1, "operation": "release", "config": config(fake, &json!({})),
        "lease": {"leaseId": lease["leaseId"], "slug": lease["slug"],
                  "name": lease["name"], "cloudId": lease["cloudId"]},
    });
    run(&request).0
}

/// `(name, outcome)` of each echoed volume.
fn outcomes(lease: &Value) -> Vec<(String, String)> {
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
fn all(want: &str) -> Vec<(String, String)> {
    vec![("npm".into(), want.into()), ("docker".into(), want.into())]
}

/// Store directory `rel` on `root`.
fn store(root: &Path, rel: &str) -> PathBuf {
    root.join("root/.crabbox-cache").join(rel)
}

/// The instance `id` as the fake holds it.
fn instance(fake: &FakeAgent37, id: &Value) -> Value {
    fake.instance(id.as_str().unwrap()).unwrap_or(Value::Null)
}

/// Entries of `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Marks the docker volume sealed on `root`, as moor's seal script does.
fn seal_docker(root: &Path) {
    std::fs::write(store(root, DOCKER).join(".crabbox-sealed"), "").unwrap();
    std::fs::write(
        store(root, "g1x/docker/var-lib-containerd/.crabbox-sealed"),
        "",
    )
    .unwrap();
}

#[test]
fn cold_acquire_links_every_path_and_echoes_empty() {
    let fake = FakeAgent37::start_with_key(KEY);
    let lease = acquire(&fake, 1, "standard");
    assert_eq!(outcomes(&lease), all("empty"));
    let root = fake.box_root(lease["cloudId"].as_str().unwrap());
    assert_eq!(
        std::fs::read_link(root.join("root/.npm")).unwrap(),
        store(&root, NPM)
    );
    assert_eq!(
        std::fs::read_link(root.join("var/lib/containerd")).unwrap(),
        store(&root, "g1x/docker/var-lib-containerd")
    );
    let daemon = std::fs::read_to_string(root.join("etc/docker/daemon.json")).unwrap();
    assert!(daemon.contains("/root/docker"), "{daemon}");
}

#[test]
fn release_keeps_a_stopped_seed_that_the_next_acquire_restores() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = acquire(&fake, 1, "standard");
    let root = fake.box_root(first["cloudId"].as_str().unwrap());
    std::fs::write(store(&root, NPM).join("pkg.tgz"), "npm data").unwrap();
    std::fs::write(store(&root, DOCKER).join("image"), "layers").unwrap();
    seal_docker(&root);
    assert_eq!(release(&fake, &first), 0);
    let seed = instance(&fake, &first["cloudId"]);
    assert_eq!(seed["status"], "stopped");
    assert_eq!(seed["metadata"]["crabbox_role"], "seed");
    let list = run(&json!({"protocolVersion": 1, "operation": "list",
                           "config": config(&fake, &json!({}))}));
    assert_eq!(list.1["leases"], json!([]), "list hides seeds");

    let second = acquire(&fake, 2, "standard");
    assert_eq!(second["cloudId"], first["cloudId"], "the seed is reused");
    assert_eq!(outcomes(&second), all("restored"));
    assert_restored(&fake, &second, &root);
}

/// The reused seed carries the new lease, its data and no seal marker.
fn assert_restored(fake: &FakeAgent37, lease: &Value, root: &Path) {
    assert_eq!(
        instance(fake, &lease["cloudId"])["name"],
        "crabbox-me--box-2"
    );
    assert!(store(root, NPM).join("pkg.tgz").exists());
    assert!(store(root, DOCKER).join("image").exists());
    assert!(
        !store(root, DOCKER).join(".crabbox-sealed").exists(),
        "restore clears the seal marker"
    );
    let resolved = run(&json!({"protocolVersion": 1, "operation": "resolve",
        "config": config(fake, &json!({})),
        "expected": {"leaseId": "cbx_2", "slug": "s2", "cloudId": lease["cloudId"]}}));
    assert_eq!(
        resolved.0, 0,
        "the reused Box resolves as the new lease: {}",
        resolved.1
    );
}

#[test]
fn a_newer_seed_replaces_older_seeds_of_its_key() {
    let fake = FakeAgent37::start_with_key(KEY);
    let a = acquire(&fake, 1, "standard");
    let b = acquire(&fake, 2, "standard");
    assert_eq!(release(&fake, &a), 0);
    assert_eq!(release(&fake, &b), 0);
    assert_eq!(
        instance(&fake, &a["cloudId"]),
        Value::Null,
        "older seed deleted"
    );
    assert_eq!(
        instance(&fake, &b["cloudId"])["metadata"]["crabbox_role"],
        "seed"
    );
}

#[test]
fn release_empties_an_unsealed_sealed_volume_and_strips_root() {
    let fake = FakeAgent37::start_with_key(KEY);
    let lease = acquire(&fake, 1, "standard");
    let root = fake.box_root(lease["cloudId"].as_str().unwrap());
    std::fs::write(store(&root, NPM).join("pkg.tgz"), "npm data").unwrap();
    std::fs::write(store(&root, DOCKER).join("image"), "unsealed").unwrap();
    for dir in ["root/work", "root/.ssh", "root/.config"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("root/.ssh/authorized_keys"), "key").unwrap();
    std::fs::write(root.join("root/.bashrc"), "x").unwrap();
    assert_eq!(release(&fake, &lease), 0);
    assert_eq!(entries(&store(&root, DOCKER)), Vec::<String>::new());
    assert!(store(&root, NPM).join("pkg.tgz").exists());
    assert_eq!(
        entries(&root.join("root")),
        [".crabbox-cache", ".npm", ".ssh", "docker"],
        "only the store, .ssh and the links into the store stay"
    );
    assert!(root.join("root/.ssh/authorized_keys").exists());
}

#[test]
fn a_failed_seeding_deletes_the_box_and_release_succeeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    let lease = acquire(&fake, 1, "standard");
    fake.set_exec_fails(true);
    assert_eq!(release(&fake, &lease), 0);
    assert_eq!(instance(&fake, &lease["cloudId"]), Value::Null);
}

#[test]
fn a_claim_lost_to_a_rival_goes_cold() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = acquire(&fake, 1, "standard");
    assert_eq!(release(&fake, &first), 0);
    fake.set_steal_claims(true);
    let second = acquire(&fake, 2, "standard");
    assert_ne!(second["cloudId"], first["cloudId"]);
    assert_eq!(outcomes(&second), all("empty"));
    assert_eq!(
        instance(&fake, &first["cloudId"])["name"],
        "crabbox-me--rival"
    );
}

#[test]
fn a_seed_of_another_shape_is_not_used() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = acquire(&fake, 1, "standard");
    assert_eq!(release(&fake, &first), 0);
    let other = acquire(&fake, 2, "fast");
    assert_ne!(other["cloudId"], first["cloudId"]);
    assert_eq!(outcomes(&other), all("empty"));
    assert_eq!(instance(&fake, &first["cloudId"])["status"], "stopped");
}

#[test]
fn cleanup_deletes_expired_and_surplus_seeds_only() {
    let fake = FakeAgent37::start_with_key(KEY);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let seeds = [
        ("old", now - 8 * 86_400),
        ("s1", now - 30),
        ("s2", now - 20),
        ("s3", now - 10),
    ];
    for (id, gen) in seeds {
        let gen = gen.to_string();
        fake.add_instance_with_metadata(
            id,
            &format!("crabbox-me--seed.{gen}"),
            "vm",
            "stopped",
            &[
                ("crabbox_owner", "me"),
                ("crabbox_role", "seed"),
                ("crabbox_cache_key", "k"),
                ("crabbox_seed_gen", &gen),
            ],
        );
    }
    let ids = |fake: &FakeAgent37| -> Vec<Value> {
        fake.instances().iter().map(|i| i["id"].clone()).collect()
    };
    let cleanup = |cache: Value| {
        let config = config(&fake, &json!({ "cache": cache }));
        run(&json!({"protocolVersion": 1, "operation": "cleanup", "config": config}))
    };
    let (code, out) = cleanup(json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["s1", "s2", "s3"], "past the 7 day ttl: {out}");
    let (code, out) = cleanup(json!({"maxSeeds": 2}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["s2", "s3"], "beyond the newest 2: {out}");
}
