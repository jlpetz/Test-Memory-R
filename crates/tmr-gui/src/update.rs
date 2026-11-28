//! Whether GitHub has a newer release than this build. The window asks and can open the release page. It does not download or install:
//! replacing a running tmr.exe is the installer's job.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::process::Command;

/// A published release: its version without a leading `v`, and the page that has it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// What the check found, against this build's version
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not asked yet
    Idle,
    Checking,
    /// This build is that release, or newer than the latest one published
    Current {
        version: String,
    },
    /// `version` is published and newer. `url` is its page.
    Available {
        version: String,
        url: String,
    },
    /// This build's version is ahead of the latest published release
    Ahead {
        version: String,
    },
    Failed(String),
}

/// The latest release GitHub lists, or why the check couldn't
pub fn fetch() -> Result<Release, String> {
    let url = api_url()?;
    let windows = std::env::var_os("SystemRoot").ok_or("Windows did not provide SystemRoot")?;
    let curl = PathBuf::from(windows).join("System32").join("curl.exe");
    let output = Command::new(curl)
        .args([
            "--disable", "--fail", "--location", "--silent", "--show-error", "--max-time", "20", "-H",
            "Accept: application/vnd.github+json", "-H", "User-Agent: TMR", &url,
        ])
        .output()
        .map_err(|e| format!("curl.exe: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if err.is_empty() { format!("the check failed ({})", output.status) } else { err });
    }
    parse_latest(&String::from_utf8_lossy(&output.stdout))
}

/// This build against a fetched release
pub fn judge(current: &str, release: &Release) -> Status {
    let Some(current) = numbers(current) else { return Status::Failed("This build's version is not a release version".to_string()) };
    let Some(latest) = numbers(&release.version) else { return Status::Failed("GitHub returned an invalid release version".to_string()) };
    match current.cmp(&latest) {
        Ordering::Less => Status::Available { version: release.version.clone(), url: release.url.clone() },
        Ordering::Equal => Status::Current { version: release.version.clone() },
        Ordering::Greater => Status::Ahead { version: release.version.clone() },
    }
}

/// The release tag from GitHub and its page in this project.
pub fn parse_latest(body: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("the release list wasn't JSON: {e}"))?;
    let tag = value.get("tag_name").and_then(|v| v.as_str()).ok_or("the release list had no tag")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if numbers(version).is_none() {
        return Err("GitHub returned an invalid release version".to_string());
    }
    let url = format!("{}/releases/tag/{tag}", env!("CARGO_PKG_REPOSITORY").trim_end_matches('/'));
    Ok(Release { version: version.to_string(), url })
}

fn api_url() -> Result<String, String> {
    let repo = env!("CARGO_PKG_REPOSITORY").trim_end_matches('/');
    let path = repo.strip_prefix("https://github.com/").ok_or("the repository isn't a github.com page")?;
    Ok(format!("https://api.github.com/repos/{path}/releases/latest"))
}

/// Compare numeric release versions, so 0.1.10 comes after 0.1.2.
fn numbers(version: &str) -> Option<[u32; 3]> {
    let [major, minor, patch] = version.split('.').collect::<Vec<_>>().try_into().ok()?;
    Some([major.parse().ok()?, minor.parse().ok()?, patch.parse().ok()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_json_is_its_version_and_page_and_a_newer_tag_is_available() {
        let body = r#"{"tag_name":"v0.1.10","html_url":"https://github.com/jlpetz/Test-Memory-R/releases/tag/v0.1.10"}"#;
        let release = parse_latest(body).unwrap();
        assert_eq!(release.version, "0.1.10");
        assert!(matches!(judge("0.1.2", &release), Status::Available { .. }));
        assert!(matches!(judge("0.1.10", &release), Status::Current { .. }));
        assert!(matches!(judge("0.2.0", &release), Status::Ahead { .. }));
        assert!(parse_latest("not json").is_err());
        assert!(parse_latest(r#"{"tag_name":"v1.2.3-rc1"}"#).is_err());
        assert_eq!(release.url, "https://github.com/jlpetz/Test-Memory-R/releases/tag/v0.1.10");
    }
}
