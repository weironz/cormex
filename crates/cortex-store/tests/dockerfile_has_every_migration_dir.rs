//! `sqlx::migrate!` 要的目录，必须都在镜像的构建上下文里。
//!
//! # 为什么这条测试值得存在
//!
//! `migrate!` 是**编译期**宏：它在编译时读目录，把每个 `.sql` 连同校验和
//! 一起烤进二进制。目录不在 `docker build` 的上下文里，编译就直接失败 ——
//! 而**本机构建永远是绿的**，因为目录就在那儿。
//!
//! v0.1.5 发版时实际撞到过：加了 `migrations-global/`（认证那套表），
//! `cargo build` / `cargo test` / CI 全绿，`docker build` 在第 299 秒挂掉，
//! 报 `error canonicalizing migration directory …/migrations-global`。
//! 也就是说，**唯一能发现它的地方是发布流水线本身**，而那是最贵的一处。
//!
//! 所以这里用一条纯文本检查把它提前到 `cargo test`：
//! 扫出源码里所有 `migrate!("…")` 的路径，逐个确认 Dockerfile 里
//! COPY 过。它拦不住所有构建上下文问题，但拦得住**这一类**
//! —— 而这一类的代价是「发版发到一半才知道」。

use std::path::Path;

/// 从 crate 目录回到仓库根。
fn repo_root() -> &'static Path {
    // crates/cortex-store → 上两级
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cortex-store 应当在 <repo>/crates/ 下面两级")
}

/// 扫出所有编译期 migration 宏引用的目录名。
///
/// 只取最后一段：宏里写的是相对 crate 的路径，而 Dockerfile 里写的是
/// 相对仓库根的。两边唯一稳定的公共部分就是目录名本身。
///
/// **只扫 `src/`**，两个理由：镜像里 `cargo build -p cortexd -p cortex-cli`
/// 根本不编译测试，所以测试用的目录不进构建上下文也无所谓；
/// 而且这个文件自己的注释里就写着那个宏名，扫到 `tests/` 会先撞上自己。
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
                // 只往 crate 根与其 src/ 里走
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

/// 每个 `migrate!` 目录都必须被 Dockerfile COPY 进去。
///
/// 断言消息要能独立读懂：这条红的时候，人多半正在发版，
/// 而下一步该做什么必须写在消息里，不能让人回来读这个文件。
#[test]
fn every_migrate_macro_directory_is_copied_into_the_image() {
    let dockerfile_path = repo_root().join("scripts/docker/Dockerfile.cortexd");
    let dockerfile = std::fs::read_to_string(&dockerfile_path)
        .unwrap_or_else(|e| panic!("读不到 {}：{e}", dockerfile_path.display()));

    let dirs = migration_dirs_referenced_in_source();
    assert!(
        !dirs.is_empty(),
        "一个 migrate! 都没扫到 —— 多半是这条测试的扫描逻辑坏了，\
         而不是真的没有 migration。这种情况下它会永远绿，等于不存在"
    );

    for dir in &dirs {
        assert!(
            dockerfile.contains(&format!("COPY {dir}/ {dir}/")),
            "源码里有 sqlx::migrate!(\"…/{dir}\")，但 {} 没有 \
             `COPY {dir}/ {dir}/`。\n\
             migrate! 是编译期宏，目录不在构建上下文里 docker build 会直接编不过，\
             而本机 cargo build 照样绿（目录就在那儿）—— 于是只有发布流水线\
             才会发现，在第 300 秒左右。\n\
             修法：在 Dockerfile 里 `COPY crates/` 附近补一行 `COPY {dir}/ {dir}/`。",
            dockerfile_path.display(),
        );
    }
}

/// migration 还要随镜像进运行时层，否则 `just prod-migrate` 在容器里跑不了。
///
/// 与上面那条分开：编不过是**立刻可见**的，而这条漏了要等到有人
/// 在生产上跑 migration 才发现「目录不存在」。
#[test]
fn every_migration_directory_also_ships_in_the_runtime_layer() {
    let dockerfile_path = repo_root().join("scripts/docker/Dockerfile.cortexd");
    let dockerfile = std::fs::read_to_string(&dockerfile_path).expect("读不到 Dockerfile");

    for dir in &migration_dirs_referenced_in_source() {
        assert!(
            dockerfile.contains(&format!("/build/{dir} /opt/cortex/{dir}")),
            "{dir}/ 编进了二进制，但没随镜像进运行时层。\n\
             `just prod-migrate` 是在容器里跑 sqlx 的，找不到目录时它报\
             「没有这个文件」——而那时人正在生产上做迁移。\n\
             修法：补一行 `COPY --from=builder /build/{dir} /opt/cortex/{dir}`。",
        );
    }
}
