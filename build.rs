//! Which build this is (TODO 72), for `build_info::BUILD`: the commit, whether tracked files had changes it doesn't hold, and the compiler.
//! The nightly is unpinned, so two builds of one commit can be different binaries, which only the compiler's line tells apart. Without git,
//! the commit reads `unknown`.

use std::path::Path;
use std::process::Command;

fn main() {
    // A command's trimmed output, `None` when it can't run or fails
    let output = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = output("git", &["rev-parse", "HEAD"]);
    // Untracked files aren't the build's. No optional locks, so the status doesn't rewrite the index, which would run this again on the
    // next build.
    let dirty = commit.is_some()
        && output("git", &["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let rustc = std::env::var("RUSTC").ok().and_then(|rustc| output(&rustc, &["-V"]));
    println!("cargo::rustc-env=TMR_COMMIT={}", commit.as_deref().unwrap_or("unknown"));
    println!("cargo::rustc-env=TMR_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo::rustc-env=TMR_RUSTC={}", rustc.as_deref().unwrap_or("unknown"));

    // Again when the commit, the index or the sources change: the engine's, and under `crates/` tmr.exe's, the CLI's and the window's,
    // which are in the binary too. Git names its files, so a worktree, whose `.git` is a file, watches its own: HEAD, the branch it names
    // (a commit moves the branch, not HEAD), packed-refs, the reflog and the index. A path that doesn't exist would run this on every
    // build, so only those that do.
    if commit.is_some() {
        let mut git_files = vec!["HEAD".to_string(), "index".to_string(), "logs/HEAD".to_string(), "packed-refs".to_string()];
        git_files.extend(output("git", &["symbolic-ref", "-q", "HEAD"]));
        for file in &git_files {
            if let Some(path) = output("git", &["rev-parse", "--git-path", file]).filter(|path| Path::new(path).exists()) {
                println!("cargo::rerun-if-changed={path}");
            }
        }
    }
    for path in ["src", "crates", "build.rs", "Cargo.toml"] {
        println!("cargo::rerun-if-changed={path}");
    }
}
