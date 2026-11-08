//! Checks GitHub for a newer release.
//!
//! One plain HTTPS request to the GitHub releases API. It carries no connection
//! details, queries or identifiers beyond the app version in the User-Agent.

use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

/// Where users download a new version.
pub const DOWNLOAD_URL: &str = "https://github.com/TupleBase/tuplebase-app/releases/latest";

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/TupleBase/tuplebase-app/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// The latest published version, if it is newer than `current`.
/// Blocks on the network; call it off the UI thread.
pub fn check(current: &str) -> Result<Option<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let release: Release = agent
        .get(LATEST_RELEASE_API)
        .header(
            "User-Agent",
            concat!("TupleBase/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    let latest = release.tag_name.trim_start_matches('v');
    Ok(is_newer(latest, current).then(|| latest.to_owned()))
}

/// `x.y.z` only, so a pre-release or malformed tag is never offered as an update.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_compare_numerically() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn malformed_versions_are_never_newer() {
        for bad in ["", "latest", "1.2", "1.2.3.4", "1.2.3-beta", "9.9.x"] {
            assert!(!is_newer(bad, "0.1.0"), "{bad}");
        }
    }

    /// Calls the real GitHub API, so it only runs on request.
    #[test]
    fn real_release_is_found() {
        if std::env::var("TUPLEBASE_IT_NET").is_err() {
            eprintln!("skipped: set TUPLEBASE_IT_NET=1");
            return;
        }
        assert!(check("0.0.1").unwrap().is_some());
        assert_eq!(check("999.0.0").unwrap(), None);
    }
}
