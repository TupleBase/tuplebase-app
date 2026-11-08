//! Checks GitHub for a newer release.
//!
//! One plain HTTPS request to the GitHub releases API. It carries no connection
//! details, queries or identifiers beyond the app version in the User-Agent.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where users download a new version.
pub const DOWNLOAD_URL: &str = "https://github.com/TupleBase/tuplebase-app/releases/latest";

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/TupleBase/tuplebase-app/releases/latest";

const INSTALLER_BASE: &str = "https://raw.githubusercontent.com/TupleBase/tuplebase-app";

const RELEASES_BASE: &str = "https://github.com/TupleBase/tuplebase-app/releases/download";

const USER_AGENT: &str = concat!("TupleBase/", env!("CARGO_PKG_VERSION"));

const INSTALLER: &str = if cfg!(windows) {
    "install.ps1"
} else {
    "install.sh"
};

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
        .header("User-Agent", USER_AGENT)
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

/// An update that has been downloaded and checked, ready to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub dir: PathBuf,
    pub version: String,
}

/// Downloads `version` and checks it against the release's checksums, reporting each
/// new percentage. Blocks on the network; call it off the UI thread.
pub fn download(version: &str, progress: impl FnMut(u8)) -> Result<Download> {
    if parse(version).is_none() {
        bail!("not a release version: {version}");
    }
    let Some(file) = release_file(version) else {
        bail!("there is no build of TupleBase for this platform");
    };
    let dir = crate::config::config_dir()?.join("update");
    fetch(
        &format!("{RELEASES_BASE}/v{version}"),
        &format!("{INSTALLER_BASE}/v{version}/{INSTALLER}"),
        &file,
        &dir,
        progress,
    )?;
    Ok(Download {
        dir,
        version: version.to_owned(),
    })
}

/// The release file for this platform; the installers use the same names.
fn release_file(version: &str) -> Option<String> {
    let platform = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-arm64.dmg"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64.tar.gz"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x64.zip"
    } else {
        return None;
    };
    Some(format!("TupleBase-{version}-{platform}"))
}

/// Fills `dir` with the installer, the checksums and the checked release file.
fn fetch(
    release_url: &str,
    installer_url: &str,
    file: &str,
    dir: &Path,
    progress: impl FnMut(u8),
) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).context("clearing an earlier download")?;
    }
    fs::create_dir_all(dir).context("creating the download folder")?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .build()
        .into();
    save(&agent, installer_url, &dir.join(INSTALLER), |_| {})?;
    let sums = dir.join("SHA256SUMS");
    save(&agent, &format!("{release_url}/SHA256SUMS"), &sums, |_| {})?;
    let sums = fs::read_to_string(&sums).context("reading the checksums")?;
    let Some(expected) = expected_checksum(&sums, file) else {
        bail!("the release has no checksum for {file}");
    };
    let actual = save(
        &agent,
        &format!("{release_url}/{file}"),
        &dir.join(file),
        progress,
    )?;
    if !actual.eq_ignore_ascii_case(expected) {
        fs::remove_dir_all(dir).ok();
        bail!("{file} doesn't match its checksum, so it wasn't installed");
    }
    Ok(())
}

/// Streams `url` into `path`, returning the SHA-256 of what was written.
fn save(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    mut progress: impl FnMut(u8),
) -> Result<String> {
    let name = url.rsplit('/').next().unwrap_or(url);
    let context = || format!("downloading {name}");
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(context)?;
    let total = response.body().content_length().filter(|t| *t > 0);
    let mut reader = response.into_body().into_reader();
    let mut out = File::create(path).with_context(context)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut received = 0;
    let mut shown = None;
    loop {
        let n = reader.read(&mut buffer).with_context(context)?;
        if n == 0 {
            break;
        }
        out.write_all(&buffer[..n]).with_context(context)?;
        hasher.update(&buffer[..n]);
        received += n as u64;
        if let Some(total) = total {
            let percent = (received * 100 / total).min(100) as u8;
            if shown != Some(percent) {
                shown = Some(percent);
                progress(percent);
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Finds a file in `sha256sum` output (`<hash>  <name>`).
fn expected_checksum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim_start().trim_start_matches('*') == file).then_some(hash)
    })
}

/// Waits for the app to exit, runs the downloaded installer, and reopens the app. The
/// old version is reopened if the install fails.
const UNIX_UPGRADE: &str = r#"
while kill -0 "$TB_PID" 2>/dev/null; do sleep 0.2; done
sh "$TUPLEBASE_DOWNLOAD_DIR/install.sh" >"$TB_LOG" 2>&1
rm -rf "$TUPLEBASE_DOWNLOAD_DIR"
if [ -n "${TUPLEBASE_APP_PATH:-}" ]; then
  exec open "$TUPLEBASE_APP_PATH"
else
  exec "$TUPLEBASE_BIN_DIR/tuplebase-app"
fi
"#;

// Piped into `iex` because Windows' default policy refuses to run a .ps1 file.
const WINDOWS_UPGRADE: &str = r#"
Wait-Process -Id $env:TB_PID -ErrorAction SilentlyContinue
try {
  Get-Content -Raw (Join-Path $env:TUPLEBASE_DOWNLOAD_DIR 'install.ps1') | iex *> $env:TB_LOG
} catch {
  $_ | Out-File -Append $env:TB_LOG
}
Remove-Item -Recurse -Force $env:TUPLEBASE_DOWNLOAD_DIR
Start-Process (Join-Path $env:TUPLEBASE_INSTALL_DIR 'tuplebase-app.exe')
"#;

/// Starts a background process that installs `download` once this app has quit.
/// Call this, then quit the app.
pub fn spawn_upgrade(installation: &Installation, download: &Download) -> Result<()> {
    let log = crate::config::config_dir()?.join("update.log");
    upgrade_command(installation, download, std::process::id(), &log)
        .spawn()
        .context("starting the update")?;
    Ok(())
}

fn upgrade_command(
    installation: &Installation,
    download: &Download,
    pid: u32,
    log: &Path,
) -> Command {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", WINDOWS_UPGRADE]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", UNIX_UPGRADE]);
        c
    };
    command
        .env("TB_PID", pid.to_string())
        .env("TB_LOG", log)
        .env("TUPLEBASE_VERSION", &download.version)
        .env("TUPLEBASE_DOWNLOAD_DIR", &download.dir)
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
        // A hidden console: without any console PowerShell exits without running the script.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    command
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
    fn only_release_versions_can_be_downloaded() {
        assert!(download("1.2; rm -rf /", |_| {}).is_err());
    }

    #[test]
    fn checksums_are_found_by_file_name() {
        let sums = "aaa  app.dmg\nbbb *app.zip\n";
        assert_eq!(expected_checksum(sums, "app.dmg"), Some("aaa"));
        assert_eq!(expected_checksum(sums, "app.zip"), Some("bbb"));
        assert_eq!(expected_checksum(sums, "other"), None);
    }

    /// Serves fixed files over real HTTP, the path `fetch` takes in production.
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let found = files.iter().find(|(name, _)| path.ends_with(name.as_str()));
                let status = if found.is_some() {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let body = found.map_or(&[][..], |(_, body)| body);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn release(body: &[u8], checksum: &str) -> String {
        serve(vec![
            (INSTALLER.into(), b"installer".to_vec()),
            (
                "SHA256SUMS".into(),
                format!("{checksum}  app.bin\n").into_bytes(),
            ),
            ("app.bin".into(), body.to_vec()),
        ])
    }

    fn fetch_from(base: &str, dir: &Path, progress: impl FnMut(u8)) -> Result<()> {
        fetch(
            base,
            &format!("{base}/{INSTALLER}"),
            "app.bin",
            dir,
            progress,
        )
    }

    #[test]
    fn a_good_download_is_saved_with_rising_progress() {
        let body = vec![7; 300 * 1024];
        let checksum = format!("{:x}", Sha256::digest(&body));
        let base = release(&body, &checksum);
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("update");
        let mut seen = Vec::new();
        fetch_from(&base, &dir, |p| seen.push(p)).unwrap();
        assert_eq!(fs::read(dir.join("app.bin")).unwrap(), body);
        assert!(dir.join(INSTALLER).exists());
        assert!(seen.is_sorted() && seen.last() == Some(&100), "{seen:?}");
    }

    #[test]
    fn a_corrupt_download_is_refused_and_removed() {
        let base = release(b"tampered", "0000");
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("update");
        let error = fetch_from(&base, &dir, |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("checksum"), "{error:#}");
        assert!(!dir.exists());
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let base = serve(vec![(INSTALLER.into(), b"installer".to_vec())]);
        let dir = tempfile::tempdir().unwrap();
        let error = fetch_from(&base, &dir.path().join("update"), |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("SHA256SUMS"), "{error:#}");
    }

    /// Runs the real upgrade script around a fake installer and a fake app.
    fn run_upgrade(installer: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let update = root.path().join("update");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&update).unwrap();
        fs::write(update.join(INSTALLER), installer).unwrap();
        fake_app(&bin);
        let mut exited = Command::new("whoami")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = exited.id();
        exited.wait().unwrap();
        let download = Download {
            dir: update,
            version: "1.2.3".into(),
        };
        let status = upgrade_command(
            &Installation::Directory(bin),
            &download,
            pid,
            &root.path().join("update.log"),
        )
        .status()
        .unwrap();
        assert!(status.success());
        root
    }

    #[cfg(unix)]
    fn fake_app(bin: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let app = bin.join("tuplebase-app");
        fs::write(&app, "#!/bin/sh\ntouch \"$(dirname \"$0\")/reopened\"\n").unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn fake_app(bin: &Path) {
        // Any program that exits straight away.
        let whoami = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32\\whoami.exe");
        fs::copy(whoami, bin.join("tuplebase-app.exe")).unwrap();
    }

    fn assert_finished(root: &Path) {
        assert!(
            !root.join("update").exists(),
            "the download should be removed"
        );
        #[cfg(unix)]
        assert!(root.join("bin/reopened").exists(), "the app should reopen");
    }

    #[test]
    fn the_upgrade_runs_the_installer_then_reopens() {
        let root = if cfg!(windows) {
            run_upgrade("Write-Output installed")
        } else {
            run_upgrade("echo installed")
        };
        assert_finished(root.path());
        let log = fs::read(root.path().join("update.log")).unwrap();
        // PowerShell may write UTF-16.
        let log: Vec<u8> = log.into_iter().filter(|b| *b != 0).collect();
        assert!(String::from_utf8_lossy(&log).contains("installed"));
    }

    #[test]
    fn a_failed_install_still_reopens_the_app() {
        let root = if cfg!(windows) {
            run_upgrade("throw 'broken'")
        } else {
            run_upgrade("exit 1")
        };
        assert_finished(root.path());
    }

    /// Downloads a real release, so it only runs on request.
    #[test]
    fn real_release_downloads() {
        if std::env::var("TUPLEBASE_IT_NET").is_err() {
            eprintln!("skipped: set TUPLEBASE_IT_NET=1");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("update");
        let file = release_file("0.2.0").unwrap();
        fetch(
            &format!("{RELEASES_BASE}/v0.2.0"),
            &format!("{INSTALLER_BASE}/v0.2.0/{INSTALLER}"),
            &file,
            &dir,
            |_| {},
        )
        .unwrap();
        assert!(dir.join(&file).metadata().unwrap().len() > 1_000_000);
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
