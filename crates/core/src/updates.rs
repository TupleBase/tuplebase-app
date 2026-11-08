//! Checks GitHub for a newer release.
//!
//! One plain HTTPS request to the GitHub releases API. It carries no connection
//! details, queries or identifiers beyond the app version in the User-Agent.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Where users download a new version.
pub const DOWNLOAD_URL: &str = "https://github.com/TupleBase/tuplebase-app/releases/latest";

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/TupleBase/tuplebase-app/releases/latest";

const INSTALLER_BASE: &str = "https://raw.githubusercontent.com/TupleBase/tuplebase-app";

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

/// Where this copy of the app is installed.
#[derive(Debug, PartialEq, Eq)]
pub enum Installation {
    /// A macOS app bundle.
    MacApp(PathBuf),
    /// A folder holding the binaries, on Linux and Windows.
    Directory(PathBuf),
}

/// Finds what an update should replace. `None` for development builds, which must not
/// be overwritten by a release.
pub fn detect_installation(exe: &Path) -> Option<Installation> {
    detect(exe, cfg!(target_os = "macos"))
}

fn detect(exe: &Path, macos: bool) -> Option<Installation> {
    if exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    if macos {
        exe.ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(|app| Installation::MacApp(app.to_path_buf()))
    } else {
        exe.parent()
            .map(|dir| Installation::Directory(dir.to_path_buf()))
    }
}

/// Waits for the app to exit, runs that release's installer, and reopens the app. The
/// old version is reopened if the install fails.
const UNIX_UPGRADE: &str = r#"
while kill -0 "$TB_PID" 2>/dev/null; do sleep 0.2; done
curl -fsSL "$TB_INSTALLER" | sh >"$TB_LOG" 2>&1
if [ -n "${TUPLEBASE_APP_PATH:-}" ]; then
  exec open "$TUPLEBASE_APP_PATH"
else
  exec "$TUPLEBASE_BIN_DIR/tuplebase-app"
fi
"#;

const WINDOWS_UPGRADE: &str = r#"
Wait-Process -Id $env:TB_PID -ErrorAction SilentlyContinue
irm $env:TB_INSTALLER | iex *> $env:TB_LOG
Start-Process (Join-Path $env:TUPLEBASE_INSTALL_DIR 'tuplebase-app.exe')
"#;

/// Starts a background process that installs `version` once this app has quit.
/// Call this, then quit the app.
pub fn spawn_upgrade(installation: &Installation, version: &str) -> Result<()> {
    if parse(version).is_none() {
        bail!("not a release version: {version}");
    }
    let windows = cfg!(windows);
    let mut command = if windows {
        let mut c = Command::new("powershell");
        c.args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            WINDOWS_UPGRADE,
        ]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", UNIX_UPGRADE]);
        c
    };
    let script = if windows { "install.ps1" } else { "install.sh" };
    command
        .env("TB_PID", std::process::id().to_string())
        .env(
            "TB_INSTALLER",
            format!("{INSTALLER_BASE}/v{version}/{script}"),
        )
        .env("TB_LOG", crate::config::config_dir()?.join("update.log"))
        .env("TUPLEBASE_VERSION", version)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match installation {
        Installation::MacApp(app) => command.env("TUPLEBASE_APP_PATH", app),
        Installation::Directory(dir) => command
            .env("TUPLEBASE_BIN_DIR", dir)
            .env("TUPLEBASE_INSTALL_DIR", dir),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().context("starting the update")?;
    Ok(())
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

    #[test]
    fn development_builds_are_never_updated() {
        assert_eq!(
            detect(Path::new("/x/repo/target/debug/tuplebase-app"), true),
            None
        );
        assert_eq!(
            detect(Path::new("/x/repo/target/debug/tuplebase-app"), false),
            None
        );
    }

    #[test]
    fn a_macos_update_replaces_the_whole_bundle() {
        let exe = Path::new("/Users/a/Applications/TupleBase.app/Contents/MacOS/tuplebase-app");
        assert_eq!(
            detect(exe, true),
            Some(Installation::MacApp(PathBuf::from(
                "/Users/a/Applications/TupleBase.app"
            )))
        );
        assert_eq!(detect(Path::new("/opt/tuplebase-app"), true), None);
    }

    #[test]
    fn other_platforms_update_the_binaries_folder() {
        assert_eq!(
            detect(Path::new("/home/a/.local/bin/tuplebase-app"), false),
            Some(Installation::Directory(PathBuf::from("/home/a/.local/bin")))
        );
    }

    #[test]
    fn only_release_versions_can_be_installed() {
        let app = Installation::Directory(PathBuf::from("/tmp/none"));
        assert!(spawn_upgrade(&app, "1.2; rm -rf /").is_err());
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
