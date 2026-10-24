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
    run_in(request, &std::env::temp_dir())
}

/// Runs the binary with `request` and `tmp` as its temp dir, which holds the host's claim
/// lock; a separate `tmp` stands for another host.
fn run_in(request: &Value, tmp: &Path) -> (i32, Value) {
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
    acquire_as(fake, "me", n, class, &std::env::temp_dir())
}

/// Acquires lease `n` of class `class` for `owner` on the host whose temp dir is `tmp`.
fn acquire_as(fake: &FakeAgent37, owner: &str, n: u32, class: &str, tmp: &Path) -> Value {
    let config = config(fake, &json!({ "owner": owner }));
    let (code, out) = run_in(
        &json!({
            "protocolVersion": 1, "operation": "acquire", "config": config,
            "desired": {"leaseId": format!("cbx_{n}"), "slug": format!("s{n}"),
                        "name": format!("box-{n}"), "class": class, "cacheVolumes": volumes()},
        }),
        tmp,
    );
    assert_eq!(code, 0, "acquire failed: {out}");
    out["lease"].clone()
}

/// Releases `lease`; returns the exit code.
fn release(fake: &FakeAgent37, lease: &Value) -> i32 {
    release_as(fake, "me", lease)
}

/// Releases `lease` for `owner`; returns the exit code.
fn release_as(fake: &FakeAgent37, owner: &str, lease: &Value) -> i32 {
    let request = json!({
        "protocolVersion": 1, "operation": "release",
        "config": config(fake, &json!({ "owner": owner })),
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

/// Requests for instance `id` with `method` and path `suffix` from request `from` on.
fn calls(fake: &FakeAgent37, from: usize, method: &str, id: &Value, suffix: &str) -> usize {
    let path = format!("/v1/instances/{}{suffix}", id.as_str().unwrap());
    fake.requests()
        .iter()
        .skip(from)
        .filter(|r| r.method == method && r.path == path)
        .count()
}

#[test]
fn two_hosts_racing_for_one_seed_leave_it_to_exactly_one_lease() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = acquire(&fake, 1, "standard");
    assert_eq!(release(&fake, &first), 0);
    let seed = first["cloudId"].clone();
    let from = fake.requests().len();
    fake.start_race();
    let hosts = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let leases: Vec<Value> = std::thread::scope(|s| {
        #[allow(
            clippy::needless_collect,
            reason = "both racers start before either is joined"
        )]
        let racers: Vec<_> = (2..=3)
            .zip(&hosts)
            .map(|(n, tmp)| {
                let fake = &fake;
                s.spawn(move || acquire_as(fake, "me", n, "standard", tmp.path()))
            })
            .collect();
        racers.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let (winners, losers): (Vec<&Value>, Vec<&Value>) =
        leases.iter().partition(|l| l["cloudId"] == seed);
    assert_eq!((winners.len(), losers.len()), (1, 1), "{leases:?}");
    assert_eq!(outcomes(winners[0]), all("restored"));
    assert_eq!(outcomes(losers[0]), all("empty"), "the loser goes cold");
    let now = instance(&fake, &seed);
    assert_eq!(now["status"], "running");
    assert_eq!(
        now["metadata"]["crabbox_lease_id"], winners[0]["leaseId"],
        "the seed carries the winner's lease"
    );
    assert_eq!(
        now["name"],
        format!("crabbox-me--{}", winners[0]["name"].as_str().unwrap())
    );
    assert_eq!(
        calls(&fake, from, "POST", &seed, "/stop"),
        0,
        "nobody stops the winner's Box"
    );
    assert_eq!(
        calls(&fake, from, "DELETE", &seed, ""),
        0,
        "nobody deletes the winner's Box"
    );
}

#[test]
fn without_the_host_claim_lock_acquire_goes_cold() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = acquire(&fake, 1, "standard");
    assert_eq!(release(&fake, &first), 0);
    let second = acquire_as(
        &fake,
        "me",
        2,
        "standard",
        Path::new("/nonexistent/crabbox-tmp"),
    );
    assert_ne!(second["cloudId"], first["cloudId"]);
    assert_eq!(outcomes(&second), all("empty"));
    assert_eq!(
        instance(&fake, &first["cloudId"])["metadata"]["crabbox_role"],
        "seed"
    );
}

#[test]
fn another_owners_seed_of_the_same_key_is_never_claimed() {
    let fake = FakeAgent37::start_with_key(KEY);
    let tmp = std::env::temp_dir();
    let theirs = acquire_as(&fake, "you", 1, "standard", &tmp);
    assert_eq!(release_as(&fake, "you", &theirs), 0);
    let mine = acquire(&fake, 2, "standard");
    assert_ne!(mine["cloudId"], theirs["cloudId"]);
    assert_eq!(outcomes(&mine), all("empty"));
    let seed = instance(&fake, &theirs["cloudId"]);
    assert_eq!(seed["status"], "stopped");
    assert_eq!(seed["metadata"]["crabbox_owner"], "you");
}

#[test]
fn a_release_deletes_older_seeds_of_its_key_only() {
    let fake = FakeAgent37::start_with_key(KEY);
    let fast = acquire(&fake, 1, "fast");
    let standard = acquire(&fake, 2, "standard");
    assert_eq!(release(&fake, &fast), 0);
    assert_eq!(release(&fake, &standard), 0);
    for lease in [&fast, &standard] {
        assert_eq!(
            instance(&fake, &lease["cloudId"])["metadata"]["crabbox_role"],
            "seed",
            "each key keeps its seed"
        );
    }
}

#[test]
fn cleanup_spares_a_seed_claimed_after_it_listed() {
    let fake = FakeAgent37::start_with_key(KEY);
    let old = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 8 * 86_400)
        .to_string();
    fake.add_instance_with_metadata(
        "s1",
        &format!("crabbox-me--seed.{old}"),
        "vm",
        "stopped",
        &[
            ("crabbox_owner", "me"),
            ("crabbox_role", "seed"),
            ("crabbox_cache_key", "k"),
            ("crabbox_seed_gen", &old),
        ],
    );
    fake.take_after_list("s1");
    let config = config(&fake, &json!({}));
    let (code, out) = run(&json!({"protocolVersion": 1, "operation": "cleanup", "config": config}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(fake.instance("s1").unwrap()["status"], "running", "{out}");
}

#[test]
fn a_fresh_box_moves_its_mounted_docker_data_into_the_store() {
    let fake = FakeAgent37::start_with_key(KEY);
    let root = fake.box_root("vm00000001");
    let docker = root.join("root/docker");
    std::fs::create_dir_all(docker.join("image")).unwrap();
    std::fs::write(docker.join("image/layer"), "pulled").unwrap();
    std::fs::write(root.join(".mounts"), format!("{}\n", docker.display())).unwrap();
    let lease = acquire(&fake, 1, "standard");
    assert_eq!(lease["cloudId"], "vm00000001");
    assert_eq!(
        std::fs::read_to_string(root.join(".unmounted"))
            .unwrap()
            .trim(),
        docker.display().to_string(),
        "the init's bind mount is detached first"
    );
    assert_eq!(std::fs::read_link(&docker).unwrap(), store(&root, DOCKER));
    assert_eq!(
        std::fs::read_to_string(store(&root, DOCKER).join("image/layer")).unwrap(),
        "pulled",
        "dockerd's data moved into the store, not deleted"
    );
}

#[test]
fn release_freezes_root_before_the_stop() {
    let fake = FakeAgent37::start_with_key(KEY);
    let lease = acquire(&fake, 1, "standard");
    let root = fake.box_root(lease["cloudId"].as_str().unwrap());
    assert_eq!(release(&fake, &lease), 0);
    assert_eq!(
        std::fs::read_to_string(root.join(".frozen"))
            .unwrap()
            .trim(),
        format!("-f {}", root.join("root").display())
    );
    assert_eq!(instance(&fake, &lease["cloudId"])["status"], "stopped");
}
