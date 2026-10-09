//! Updates from the project's GitHub releases.
//!
//! Every [`CHECK_EVERY`] (and soon after starting) the daemon asks GitHub for the latest
//! release. When it's newer and automatic updates are on, it downloads the Linux bundle and its
//! signature, checks the signature against the key built into this program (so neither a
//! tampered download nor a compromised GitHub account can install anything), installs it where
//! this copy lives, and restarts into the new version. Installs owned by the system (a package
//! under /usr) aren't touched: the user is told to update with their package manager.
//!
//! Downloads go through `curl` and unpacking through `tar` (both on every desktop Linux).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Notify;
use tracing::{info, warn};

pub const REPO: &str = "ABHILESH1412/pairly";
/// The release signing key's public half (minisign format).
const PUBLIC_KEY: &str = include_str!("../../../../keys/update.pub");
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const FIRST_CHECK: Duration = Duration::from_secs(90);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where an update stands, for the app's Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    /// A newer version exists (and isn't being installed automatically).
    Available(String),
    Installing(String),
    /// Installed: the daemon restarts into it.
    Installed(String),
    /// A newer version exists, but this copy belongs to the system's package manager.
    Managed(String),
    Failed(String),
}

impl Status {
    /// (state, detail) for D-Bus.
    pub fn describe(&self) -> (&'static str, String) {
        match self {
            Self::Idle => ("idle", String::new()),
            Self::Checking => ("checking", String::new()),
            Self::UpToDate => ("up-to-date", String::new()),
            Self::Available(v) => ("available", v.clone()),
            Self::Installing(v) => ("installing", v.clone()),
            Self::Installed(v) => ("installed", v.clone()),
            Self::Managed(v) => ("managed", v.clone()),
            Self::Failed(why) => ("failed", why.clone()),
        }
    }
}

pub struct Updater {
    status: Mutex<Status>,
    auto: Mutex<bool>,
    settings: PathBuf,
    downloads: PathBuf,
    /// For signals and notifications; set once the daemon is on the bus.
    conn: std::sync::OnceLock<zbus::Connection>,
    /// Restart the daemon (into the new version).
    restart: Arc<Notify>,
    /// Wakes the loop for a check now.
    check_now: Notify,
    /// Run one install at a time.
    busy: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// `1.2.3` (or `v1.2.3`) as numbers, for comparing.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').split('.');
    let n = |p: Option<&str>| p.and_then(|s| s.split('-').next()?.parse().ok());
    Some((n(parts.next())?, n(parts.next())?, n(parts.next())?))
}

fn newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// Whether `data` carries a valid signature by the release key.
fn verify(data: &[u8], signature: &str, key: &str) -> Result<(), String> {
    let key = minisign_verify::PublicKey::from_base64(
        key.lines()
            .find(|l| !l.starts_with("untrusted comment:") && !l.trim().is_empty())
            .ok_or("no public key")?
            .trim(),
    )
    .map_err(|e| format!("bad public key: {e}"))?;
    let signature =
        minisign_verify::Signature::decode(signature).map_err(|e| format!("bad signature: {e}"))?;
    key.verify(data, &signature, false)
        .map_err(|_| "the download's signature doesn't match: not installed".to_owned())
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--retry", "2", "--max-time", "600"])
        .args(args)
        .output()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "download failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The install prefix of this copy (`~/.local` for `~/.local/bin/pairlyd`), if the user owns it.
fn user_prefix() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let bin = exe.parent()?;
    let prefix = bin.parent()?;
    (exe.starts_with(&home) && bin.file_name()? == "bin").then(|| prefix.to_path_buf())
}

impl Updater {
    pub fn new(data_dir: &Path, cache_dir: &Path, restart: Arc<Notify>) -> Arc<Self> {
        let settings = data_dir.join("auto-update");
        // On unless switched off.
        let auto = std::fs::read_to_string(&settings).map_or(true, |s| s.trim() != "off");
        Arc::new(Self {
            status: Mutex::new(Status::Idle),
            auto: Mutex::new(auto),
            settings,
            downloads: cache_dir.join("updates"),
            conn: std::sync::OnceLock::new(),
            restart,
            check_now: Notify::new(),
            busy: tokio::sync::Mutex::new(()),
        })
    }

    pub fn set_conn(&self, conn: zbus::Connection) {
        let _ = self.conn.set(conn);
    }

    fn notify(&self, summary: String, body: &str) {
        if let Some(conn) = self.conn.get() {
            crate::notifications::show_simple(conn, summary, body);
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn auto(&self) -> bool {
        *self.auto.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_auto(&self, on: bool) {
        *self.auto.lock().unwrap_or_else(PoisonError::into_inner) = on;
        let _ = std::fs::write(&self.settings, if on { "on\n" } else { "off\n" });
        if on {
            // An update already found installs now.
            self.check_now.notify_one();
        }
    }

    pub fn check_now(&self) {
        self.check_now.notify_one();
    }

    /// Install the update found (when automatic updates are off).
    pub fn install_now(self: &Arc<Self>) {
        if let Status::Available(version) = self.status() {
            let this = self.clone();
            tokio::spawn(async move { this.install(&version).await });
        }
    }

    async fn set_status(&self, status: Status) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
        if let Some(conn) = self.conn.get()
            && let Ok(emitter) =
                zbus::object_server::SignalEmitter::new(conn, pairly_dbus::OBJECT_PATH)
        {
            let _ = crate::dbus::DaemonIface::update_changed(&emitter).await;
        }
    }

    /// Check now and then, for as long as the daemon runs.
    pub async fn run(self: Arc<Self>) {
        tokio::select! {
            () = tokio::time::sleep(FIRST_CHECK) => {}
            () = self.check_now.notified() => {}
        }
        loop {
            self.check().await;
            tokio::select! {
                () = tokio::time::sleep(CHECK_EVERY) => {}
                () = self.check_now.notified() => {}
            }
        }
    }

    async fn check(self: &Arc<Self>) {
        let _busy = self.busy.lock().await;
        self.set_status(Status::Checking).await;
        let release = tokio::task::spawn_blocking(|| {
            let body = curl(&[
                "-H",
                "Accept: application/vnd.github+json",
                &format!("https://api.github.com/repos/{REPO}/releases/latest"),
            ])?;
            serde_json::from_slice::<Release>(&body).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        let release = match release {
            Ok(r) if !r.draft && !r.prerelease => r,
            Ok(_) => return self.set_status(Status::UpToDate).await,
            Err(e) => {
                warn!(error = %e, "update check failed");
                return self.set_status(Status::Failed(e)).await;
            }
        };
        let latest = release.tag_name.trim_start_matches('v').to_owned();
        if !newer(&latest, VERSION) {
            return self.set_status(Status::UpToDate).await;
        }
        info!(latest, current = VERSION, "an update is available");
        if user_prefix().is_none() {
            self.set_status(Status::Managed(latest.clone())).await;
            self.notify(
                format!("Pairly {latest} is available"),
                "Update it with your package manager.",
            );
            return;
        }
        if self.auto() {
            drop(_busy);
            self.install_from(&latest, &release).await;
        } else {
            self.set_status(Status::Available(latest.clone())).await;
            self.notify(
                format!("Pairly {latest} is available"),
                "Open Pairly → Settings to install it.",
            );
        }
    }

    async fn install(self: &Arc<Self>, version: &str) {
        let release = tokio::task::spawn_blocking(|| {
            let body = curl(&[
                "-H",
                "Accept: application/vnd.github+json",
                &format!("https://api.github.com/repos/{REPO}/releases/latest"),
            ])?;
            serde_json::from_slice::<Release>(&body).map_err(|e| e.to_string())
        })
        .await;
        match release {
            Ok(Ok(r)) => self.install_from(version, &r).await,
            Ok(Err(e)) => self.set_status(Status::Failed(e)).await,
            Err(e) => self.set_status(Status::Failed(e.to_string())).await,
        }
    }

    async fn install_from(self: &Arc<Self>, version: &str, release: &Release) {
        let _busy = self.busy.lock().await;
        let Some(prefix) = user_prefix() else { return };
        let bundle = format!("pairly-{version}-linux-x86_64");
        let url = |name: &str| {
            release
                .assets
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.browser_download_url.clone())
        };
        let (Some(archive), Some(signature)) = (
            url(&format!("{bundle}.tar.gz")),
            url(&format!("{bundle}.tar.gz.minisig")),
        ) else {
            return self
                .set_status(Status::Failed(
                    "the release has no signed Linux bundle".into(),
                ))
                .await;
        };
        self.set_status(Status::Installing(version.to_owned()))
            .await;
        let dir = self.downloads.clone();
        let bundle_name = bundle.clone();
        let result = tokio::task::spawn_blocking(move || {
            install_bundle(&dir, &bundle_name, &archive, &signature, &prefix)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        match result {
            Ok(()) => {
                info!(version, "update installed: restarting");
                self.set_status(Status::Installed(version.to_owned())).await;
                self.notify(
                    format!("Pairly updated to {version}"),
                    "Reopen the Pairly window to use the new version.",
                );
                self.restart.notify_one();
            }
            Err(e) => {
                warn!(error = %e, "update failed");
                self.set_status(Status::Failed(e)).await;
            }
        }
    }
}

/// Download, verify, unpack and install a bundle under `prefix`.
fn install_bundle(
    dir: &Path,
    bundle: &str,
    archive_url: &str,
    signature_url: &str,
    prefix: &Path,
) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let signature = String::from_utf8(curl(&[signature_url])?).map_err(|e| e.to_string())?;
    let archive = curl(&[archive_url])?;
    verify(&archive, &signature, PUBLIC_KEY)?;
    let file = dir.join(format!("{bundle}.tar.gz"));
    std::fs::write(&file, &archive).map_err(|e| e.to_string())?;
    let ok = Command::new("tar")
        .arg("-xzf")
        .arg(&file)
        .arg("-C")
        .arg(dir)
        .status()
        .map_err(|e| format!("can't run tar: {e}"))?
        .success();
    if !ok {
        return Err("couldn't unpack the update".into());
    }
    let unpacked = dir.join(bundle);
    // The bundle's own Makefile copies files only (the binaries are prebuilt).
    let installed = Command::new("make")
        .arg("-C")
        .arg(&unpacked)
        .arg("install")
        .arg(format!("PREFIX={}", prefix.display()))
        .status()
        .is_ok_and(|s| s.success());
    if !installed {
        // No `make`: the programs are what matters.
        let bin = prefix.join("bin");
        for name in ["pairlyd", "pairly", "pairly-gtk"] {
            let from = unpacked.join("target/release").join(name);
            // Copy beside, then rename over: a running program keeps its old file.
            let temp = bin.join(format!(".{name}.new"));
            std::fs::copy(&from, &temp).map_err(|e| format!("installing {name}: {e}"))?;
            std::fs::rename(&temp, bin.join(name))
                .map_err(|e| format!("installing {name}: {e}"))?;
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(newer("v0.1.4", "0.1.3"));
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(!newer("v0.1.3", "0.1.3"));
        assert!(!newer("0.1.2", "0.1.3"));
        assert!(!newer("garbage", "0.1.3"));
    }

    // A throwaway key, only for this test (not the release key).
    const TEST_KEY: &str = include_str!("../tests/update-test.pub");
    const TEST_SIG: &str = include_str!("../tests/update-test.txt.minisig");

    #[test]
    fn the_release_key_is_a_real_key() {
        let line = PUBLIC_KEY
            .lines()
            .find(|l| !l.starts_with("untrusted comment:") && !l.trim().is_empty())
            .expect("a key line");
        assert!(
            minisign_verify::PublicKey::from_base64(line.trim()).is_ok(),
            "keys/update.pub must hold the update key's public half"
        );
    }

    #[test]
    fn signatures_are_checked() {
        assert_eq!(verify(b"pairly update test\n", TEST_SIG, TEST_KEY), Ok(()));
        assert!(verify(b"pairly update tesT\n", TEST_SIG, TEST_KEY).is_err());
        assert!(verify(b"pairly update test\n", TEST_SIG, PUBLIC_KEY).is_err());
    }
}
