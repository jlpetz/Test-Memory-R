//! Which build this is (TODO 72), for `build_info::BUILD`: the commit, whether tracked files had changes it doesn't hold, and the compiler.
//! The nightly is unpinned, so two builds of one commit can be different binaries, which only the compiler's line tells apart. Without git,
//! the commit reads `unknown`. A crate from crates.io (TODO 104) has no git, but the commit it was packaged from, in its
//! `.cargo_vcs_info.json`.
//!
//! And whether docs.rs is building it (`tmr_docs_rs`), which lets its build through the baseline check (`src/lib.rs`); and the target
//! features the build enables (`TMR_TARGET_FEATURES`), for `cpu_check`'s test that it checks each.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(tmr_docs_rs)");
    println!("cargo::rerun-if-env-changed=DOCS_RS");
    if std::env::var_os("DOCS_RS").is_some() {
        println!("cargo::rustc-cfg=tmr_docs_rs");
    }
    println!("cargo::rustc-env=TMR_TARGET_FEATURES={}", std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default());

    // A command's trimmed output, `None` when it can't run or fails
    let output = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // A packaged crate's commit is the one it was packaged from: git there would find whatever repo holds the crate (this one while
    // `cargo package` verifies it, or none)
    let packaged = std::fs::read_to_string(".cargo_vcs_info.json").ok().map(|info| packaged_commit(&info));
    let is_packaged = packaged.is_some();
    let (commit, dirty) = match packaged {
        Some(packaged) => packaged,
        None => {
            let commit = output("git", &["rev-parse", "HEAD"]);
            // Untracked files aren't the build's. No optional locks, so the status doesn't rewrite the index, which would run this again
            // on the next build.
            let dirty = commit.is_some()
                && output("git", &["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            (commit, dirty)
        }
    };
    let rustc = std::env::var("RUSTC").ok().and_then(|rustc| output(&rustc, &["-V"]));
    println!("cargo::rustc-env=TMR_COMMIT={}", commit.as_deref().unwrap_or("unknown"));
    println!("cargo::rustc-env=TMR_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo::rustc-env=TMR_RUSTC={}", rustc.as_deref().unwrap_or("unknown"));

    // Again when the commit, the index or the sources change: the engine's, and under `crates/` tmr.exe's, the CLI's and the window's,
    // which are in the binary too. Git names its files, so a worktree, whose `.git` is a file, watches its own: HEAD, the branch it names
    // (a commit moves the branch, not HEAD), packed-refs, the reflog and the index. A path that doesn't exist would run this on every
    // build, so only those that do: a packaged crate has no `crates/`.
    if commit.is_some() && !is_packaged {
        let mut git_files = vec!["HEAD".to_string(), "index".to_string(), "logs/HEAD".to_string(), "packed-refs".to_string()];
        git_files.extend(output("git", &["symbolic-ref", "-q", "HEAD"]));
        for file in &git_files {
            if let Some(path) = output("git", &["rev-parse", "--git-path", file]).filter(|path| Path::new(path).exists()) {
                println!("cargo::rerun-if-changed={path}");
            }
        }
    }
    for path in ["src", "crates", "build.rs", "Cargo.toml"].into_iter().filter(|path| Path::new(path).exists()) {
        println!("cargo::rerun-if-changed={path}");
    }
}

/// The commit a packaged crate's `.cargo_vcs_info.json` names (`"sha1": "<hash>"`), and whether its tree had changes (`"dirty": true`,
/// there only when it had). Read by hand: a build script with no dependencies builds faster than one with serde.
fn packaged_commit(info: &str) -> (Option<String>, bool) {
    let sha1 = info.split_once("\"sha1\"").and_then(|(_, rest)| rest.split('"').nth(1)).map(str::to_string);
    let dirty = info.split_once("\"dirty\"").is_some_and(|(_, rest)| rest.trim_start_matches([':', ' ']).starts_with("true"));
    (sha1, dirty)
}
