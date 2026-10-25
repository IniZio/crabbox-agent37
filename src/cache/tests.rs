#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use super::*;

/// A request entry.
fn req(group: &str, name: &str, paths: &[&str], sealed: bool) -> CacheVolumeRequest {
    CacheVolumeRequest {
        group: group.into(),
        name: name.into(),
        key: format!("{group}-{name}"),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        size_gb: 0,
        sealed,
    }
}

/// Shape with the given template.
const fn shape(cpu: u32, template: Option<&str>) -> Shape<'_> {
    Shape {
        cpu,
        memory_gb: 4,
        disk_gb: 12,
        template,
        region: "us",
    }
}

#[test]
fn guest_paths_follow_the_allow_list() {
    for ok in [
        "~/.npm",
        "~/.gradle/caches",
        "/opt/sdk",
        "/var/cache/apt",
        "/var/lib/containerd",
        "/var/lib/containers/storage",
    ] {
        assert!(GuestPath::parse(ok).is_ok(), "{ok}");
    }
    for bad in [
        "~/.ssh",
        "~/.ssh/keys",
        "~/.crabbox-cache",
        "~/.crabbox-cache/x",
        "/opt",
        "/var/cache",
        "/etc/x",
        "~/a/../b",
        "relative",
        "~/",
    ] {
        assert!(GuestPath::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(GuestPath::parse("~/docker").unwrap().abs, "/root/docker");
}

#[test]
fn data_dir_names_are_plain_segments() {
    assert_eq!(
        data_dir_name("/opt/android-sdk"),
        "opt-android-sdk-aa51df94"
    );
    assert_eq!(
        data_dir_name("~/.cache/go-build"),
        "cache-go-build-8859d6e1"
    );
    let (dashed, nested) = (data_dir_name("/opt/a-b"), data_dir_name("/opt/a/b"));
    assert!(
        dashed.starts_with("opt-a-b-") && nested.starts_with("opt-a-b-"),
        "the slugs agree"
    );
    assert_ne!(dashed, nested, "the hash separates them");
    let presets = [
        "~/.cache/npm",
        "~/.cache/yarn",
        "~/.local/share/pnpm/store",
        "~/.bun/install/cache",
        "~/docker",
        "/var/lib/containerd",
        "~/.local/share/containers",
        "~/.gradle/caches",
        "~/.gradle/wrapper",
        "~/.pub-cache",
        "~/.cargo/registry",
        "~/.cargo/git",
        "~/.rustup",
        "~/go/pkg/mod",
        "~/.cache/go-build",
    ];
    for path in presets {
        let name = data_dir_name(path);
        let plain = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        assert!(plain && !name.starts_with('.'), "{path} -> {name}");
    }
}

#[test]
fn plan_skips_bad_and_duplicate_entries() {
    let plan = Plan::new(&[
        req("g1", "npm", &["~/.npm"], false),
        req("g1", "npm", &["~/.other"], false),
        req("g2", "again", &["~/.npm"], false),
        req("g/1", "bad", &["~/.x"], false),
        req("g1", "ssh", &["~/.ssh"], false),
    ]);
    let echo = plan.echo(&[], None);
    let outcomes: Vec<(&str, &str)> = echo
        .iter()
        .map(|e| (e.name.as_str(), e.outcome.as_str()))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("npm", "empty"),
            ("npm", "skipped"),
            ("again", "skipped"),
            ("bad", "skipped"),
            ("ssh", "skipped"),
        ]
    );
}

#[test]
fn seed_key_covers_scope_owner_shape_template_and_region() {
    const A: &str = "0123456789abcdef";
    let base = seed_key(A, "me", &shape(2, None));
    assert_eq!(base.len(), 16 + 1 + 16, "scope, dash, hash");
    assert_eq!(super::meta::scope_of(&base), A, "the scope stays readable");
    assert_eq!(base, seed_key(A, "me", &shape(2, None)));
    assert_ne!(base, seed_key("fedcba9876543210", "me", &shape(2, None)));
    assert_ne!(base, seed_key(A, "you", &shape(2, None)));
    assert_ne!(base, seed_key(A, "me", &shape(4, None)));
    assert_ne!(base, seed_key(A, "me", &shape(2, Some("custom"))));
    let eu = Shape {
        region: "eu",
        ..shape(2, None)
    };
    assert_ne!(base, seed_key(A, "me", &eu), "a seed never crosses regions");
}

#[test]
fn only_sixteen_lowercase_hex_is_a_scope() {
    let mut diag = Vec::new();
    assert_eq!(
        scope(Some("0123456789abcdef"), &mut diag),
        Some("0123456789abcdef")
    );
    assert!(diag.is_empty());
    assert_eq!(scope(None, &mut diag), None);
    assert!(diag.is_empty(), "an absent scope is not reported");
    for bad in [
        "0123456789ABCDEF",
        "0123456789abcde",
        "0123456789abcdef0",
        "g123456789abcdef",
        "",
    ] {
        let mut diag = Vec::new();
        assert_eq!(scope(Some(bad), &mut diag), None, "{bad}");
        assert!(
            String::from_utf8(diag).unwrap().contains("cacheScope"),
            "{bad}"
        );
    }
}

#[test]
fn echo_marks_restored_names_with_the_generation() {
    let plan = Plan::new(&[
        req("g1", "npm", &["~/.npm"], false),
        req("g1", "yarn", &["~/.cache/yarn"], false),
    ]);
    let echo = plan.echo(&["npm".to_owned()], Some("1700000000"));
    assert_eq!(
        (echo[0].outcome.as_str(), echo[0].detail.as_str()),
        ("restored", "gen 1700000000")
    );
    assert_eq!(echo[1].outcome, "empty");
}
