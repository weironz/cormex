//! The npm package's two moving parts, checked without npm (P6-5).
//!
//! # Why this is a Rust test rather than a Node one
//!
//! The repository has no Node test runner, and adding one for two files would
//! be a second toolchain in CI for a package that ships six binaries. What has
//! to be checked is small and static: the manifest's `optionalDependencies`
//! must name the same six platforms `publish.mjs` generates, and the shim must
//! map Node's `platform`/`arch` onto those names.
//!
//! The failure this catches is the one that is invisible until a user hits it:
//! a typo in one of the six names means npm installs nothing on that platform,
//! and the error the user sees is "no prebuilt binary", which reads like "this
//! platform is not supported".

/// `(Node platform, Node arch)` → the npm package name.
const EXPECTED: &[(&str, &str)] = &[
    ("linux", "x64"),
    ("linux", "arm64"),
    ("darwin", "x64"),
    ("darwin", "arm64"),
    ("win32", "x64"),
    ("win32", "arm64"),
];

/// Read a repo-relative path.
///
/// `cargo test` runs with the crate's directory as the working directory, so a
/// bare `npm/package.json` resolves to `crates/cormex/npm/package.json` — which
/// does not exist, and the failure reads like "the file was deleted".
fn read(path: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("cormex sits two levels below the repository root");
    let full = root.join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("cannot read {}: {e}", full.display()))
}

/// The main package's `optionalDependencies` name exactly the six platforms the
/// publisher generates.
#[test]
fn the_manifest_names_every_platform_the_publisher_generates() {
    let manifest = read("npm/package.json");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");

    let deps = json["optionalDependencies"]
        .as_object()
        .expect("optionalDependencies should be an object");
    assert_eq!(
        deps.len(),
        EXPECTED.len(),
        "the manifest names {} platforms, the publisher generates {}",
        deps.len(),
        EXPECTED.len()
    );
    for (os, cpu) in EXPECTED {
        let name = format!("@cormex/cli-{os}-{cpu}");
        assert!(
            deps.contains_key(&name),
            "the manifest does not name {name}; npm would install nothing on {os}-{cpu} and the \
             user would see \"no prebuilt binary for {os}-{cpu}\""
        );
    }

    // The `bin` entry has to point at a file that exists, or `npm install -g`
    // creates a command that fails at the first run
    let bin = json["bin"]["cormex"]
        .as_str()
        .expect("a bin entry named cormex");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("cormex sits two levels below the repository root");
    assert!(
        root.join("npm").join(bin).exists(),
        "the bin entry points at npm/{bin}, which does not exist"
    );
}

/// `publish.mjs`'s target table covers the same six platforms.
///
/// Read as text rather than executed: running it would need a real `npm` and
/// six real artefacts, and what can go wrong here is a typo in a name, which a
/// substring check finds.
#[test]
fn the_publisher_covers_every_platform() {
    let script = read("npm/publish.mjs");
    for (os, cpu) in EXPECTED {
        // The table is `{ os: 'linux', cpu: 'x64' }` per Rust target
        let needle = format!("os: '{os}', cpu: '{cpu}'");
        assert!(
            script.contains(&needle),
            "publish.mjs has no entry for {needle}; that platform would be skipped silently"
        );
    }
}

/// The shim builds the package name from Node's own `platform`/`arch`.
#[test]
fn the_shim_uses_the_node_platform_names() {
    let shim = read("npm/bin/cormex.js");
    assert!(
        shim.contains("process.platform") && shim.contains("process.arch"),
        "the shim should derive the package name from Node's own values"
    );
    assert!(
        shim.contains("@cormex/cli-${platform}-${arch}"),
        "the shim's package name does not match the manifest's naming scheme"
    );
    // **The exit code has to be passed through.** A shim that exits 0 on a
    // failed capture would make `cormex sync` look successful in a cron job
    assert!(
        shim.contains("result.status"),
        "the shim does not propagate the binary's exit code"
    );
}

/// The version in `package.json` is a placeholder, and the publisher replaces
/// it.
///
/// Checked rather than assumed: a manifest that carried a real version would
/// mean `npm publish` could ship a package whose version disagrees with the tag.
#[test]
fn the_manifest_version_is_a_placeholder() {
    let manifest = read("npm/package.json");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
    assert_eq!(
        json["version"].as_str(),
        Some("0.0.0"),
        "npm/package.json carries a real version; the publisher sets it from the tag, and two \
         sources for it is one source that drifts"
    );
}
