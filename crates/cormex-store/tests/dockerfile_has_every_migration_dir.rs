//! Every directory `sqlx::migrate!` needs must be inside the image's build
//! context.
//!
//! # Why this test is worth having
//!
//! `migrate!` is a **compile-time** macro: it reads the directory at compile
//! time and bakes every `.sql` into the binary along with its checksum. If the
//! directory is not in `docker build`'s context, compilation fails outright —
//! while **the local build is always green**, because the directory is right
//! there.
//!
//! This actually happened while releasing v0.1.5: `migrations-global/` (the
//! auth tables) was added, `cargo build` / `cargo test` / CI were all green,
//! and `docker build` died at the 299th second reporting
//! `error canonicalizing migration directory …/migrations-global`. In other
//! words, **the only place that could discover it was the release pipeline
//! itself**, which is the most expensive place there is.
//!
//! So a plain-text check moves it forward to `cargo test`: scan the source for
//! every `migrate!("…")` path and confirm one by one that the Dockerfile COPYs
//! it. It cannot stop every build-context problem, but it stops **this class**
//! — and the cost of this class is "you find out halfway through a release".

use std::path::Path;

/// Walk back from the crate directory to the repository root.
fn repo_root() -> &'static Path {
    // crates/cormex-store → two levels up
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cormex-store should be two levels below <repo>/crates/")
}

/// Scan out the directory names referenced by every compile-time migration
/// macro.
///
/// Only the last segment is taken: the macro writes a path relative to the
/// crate, while the Dockerfile writes one relative to the repository root. The
/// only stable part common to both is the directory name itself.
///
/// **Only `src/` is scanned**, for two reasons: the image's
/// `cargo build -p cormexd -p cormex-cli` does not compile tests at all, so it
/// does not matter whether test-only directories are in the build context; and
/// this file's own comments contain that macro name, so scanning `tests/` would
/// hit itself first.
fn migration_dirs_referenced_in_source() -> Vec<String> {
    let mut dirs = Vec::new();
    let mut stack = vec![repo_root().join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Only descend into crate roots and their src/
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if name == "target" || name == "tests" || name == "benches" || name == "examples" {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let Ok(src) = std::fs::read_to_string(&p) else {
                    continue;
                };
                for rest in src.split("migrate!(\"").skip(1) {
                    let Some(path) = rest.split('"').next() else {
                        continue;
                    };
                    if let Some(name) = path.rsplit('/').next()
                        && !name.is_empty()
                    {
                        dirs.push(name.to_string());
                    }
                }
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Every `migrate!` directory must be COPYed in by the Dockerfile.
///
/// The assertion messages have to be readable standalone: when this goes red,
/// whoever is looking is most likely mid-release, and what to do next must be in
/// the message rather than requiring them to come back and read this file.
#[test]
fn every_migrate_macro_directory_is_copied_into_the_image() {
    let dockerfile_path = repo_root().join("scripts/docker/Dockerfile.cormexd");
    let dockerfile = std::fs::read_to_string(&dockerfile_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dockerfile_path.display()));

    let dirs = migration_dirs_referenced_in_source();
    assert!(
        !dirs.is_empty(),
        "not a single migrate! was found -- most likely this test's scanning logic is broken \
         rather than there genuinely being no migrations. In that state it would be green \
         forever, which is the same as not existing"
    );

    for dir in &dirs {
        assert!(
            dockerfile.contains(&format!("COPY {dir}/ {dir}/")),
            "the source contains sqlx::migrate!(\"…/{dir}\"), but {} has no \
             `COPY {dir}/ {dir}/`.\n\
             migrate! is a compile-time macro, so if the directory is not in the build context \
             docker build fails to compile outright, while the local cargo build stays green \
             (the directory is right there) -- meaning only the release pipeline finds out, \
             around the 300th second.\n\
             Fix: add a `COPY {dir}/ {dir}/` line near `COPY crates/` in the Dockerfile.",
            dockerfile_path.display(),
        );
    }
}

/// The migrations must also ship into the runtime layer of the image, or
/// `just prod-migrate` cannot run inside the container.
///
/// Kept separate from the one above: a compile failure is **immediately
/// visible**, whereas missing this one is not discovered until somebody runs a
/// migration in production and finds "the directory does not exist".
#[test]
fn every_migration_directory_also_ships_in_the_runtime_layer() {
    let dockerfile_path = repo_root().join("scripts/docker/Dockerfile.cormexd");
    let dockerfile = std::fs::read_to_string(&dockerfile_path).expect("cannot read the Dockerfile");

    for dir in &migration_dirs_referenced_in_source() {
        assert!(
            dockerfile.contains(&format!("/build/{dir} /opt/cormex/{dir}")),
            "{dir}/ is compiled into the binary but does not ship into the image's runtime \
             layer.\n\
             `just prod-migrate` runs sqlx inside the container, and when it cannot find the \
             directory it reports \"no such file\" -- at the moment somebody is performing a \
             migration in production.\n\
             Fix: add a `COPY --from=builder /build/{dir} /opt/cormex/{dir}` line.",
        );
    }
}
