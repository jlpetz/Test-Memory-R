//! Which build this binary is (TODO 72): its version, and the commit, compiler and profile it was built with (`build.rs`). The results file
//! records it, and the log file's header.

/// A build of TMR.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    /// The package's version, `0.1.0`
    pub version: &'static str,
    /// The commit built (`git rev-parse HEAD`), `unknown` without git
    pub commit: &'static str,
    /// Whether tracked files had changes the commit doesn't hold
    pub dirty: bool,
    /// `rustc -V`: the nightly is unpinned, so one commit can be built by several compilers
    pub rustc: &'static str,
    /// `release` or `debug`, as the console's Build Type says
    pub profile: &'static str,
}

/// This binary's build.
pub const BUILD: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    commit: env!("TMR_COMMIT"),
    dirty: matches!(env!("TMR_DIRTY").as_bytes(), b"1"),
    rustc: env!("TMR_RUSTC"),
    profile: if cfg!(debug_assertions) { "debug" } else { "release" },
};

impl BuildInfo {
    /// All but the version, for the log file's header: `commit 25ff780… (dirty), rustc 1.101.0-nightly (1a2c4797e 2026-10-21), release`.
    pub fn describe(&self) -> String {
        format!("commit {}{}, {}, {}", self.commit, if self.dirty { " (dirty)" } else { "" }, self.rustc, self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build's version is the package's, as the literals it replaced were; its commit is a hash, or `unknown` without git; `describe`
    /// names the rest.
    #[test]
    fn the_build_names_its_version_commit_and_compiler() {
        assert_eq!(BUILD.version, env!("CARGO_PKG_VERSION"));
        let hash = BUILD.commit.len() >= 40 && BUILD.commit.bytes().all(|b| b.is_ascii_hexdigit());
        assert!(hash || BUILD.commit == "unknown", "{}", BUILD.commit);
        assert!(BUILD.rustc.starts_with("rustc ") || BUILD.rustc == "unknown", "{}", BUILD.rustc);
        let line = BUILD.describe();
        assert!(line.starts_with(&format!("commit {}", BUILD.commit)) && line.ends_with(&format!(", {}", BUILD.profile)), "{line}");
    }
}
