//! Starting Pairly's screen helper on phones plugged in with USB debugging, the way scrcpy
//! starts its server: through `adb`, as Android's "shell" user. The helper then shares the
//! screen without Android's "Start recording?" prompt and types and taps for real, over any
//! link (Wi-Fi or the internet relay), until the phone restarts.
//!
//! Optional: without `adb`, or with no phone authorised for USB debugging, nothing happens and
//! the phone asks as usual. Each PC app says how to run `adb` (through a Flatpak sandbox, with
//! no console window on Windows).

use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info};

const PACKAGES: [&str; 2] = [
    "io.github.abhilesh1412.pairly",
    "io.github.abhilesh1412.pairly.dev",
];
const HELPER: &str = "dev.pairly.android.helper.Helper";
/// Stops a helper already running (maybe an older one). Not `pkill -f`: that matches the shell
/// running this very command (its command line names the helper too) and kills it first.
const STOP_HELPER: &str = r#"for p in $(pidof app_process); do case "$(tr "\0" " " < /proc/$p/cmdline)" in *dev.pairly.android.helper.Helper*) kill $p;; esac; done;"#;
/// Every adb call is quick; a stuck one (a phone asking to authorise) mustn't hold things up.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `adb <args>`'s output, or None if adb is missing, fails or takes too long.
async fn adb(command: &(dyn Fn() -> Command + Sync), args: &[&str]) -> Option<String> {
    let run = command().args(args).kill_on_drop(true).output();
    let out = tokio::time::timeout(TIMEOUT, run).await.ok()?.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Phones adb may use now (plugged in and authorised).
fn ready_devices(list: &str) -> Vec<String> {
    list.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let serial = f.next()?;
            (f.next()? == "device").then(|| serial.to_owned())
        })
        .collect()
}

/// `package:<name> uid:<uid>[,<uid>…]` lines → Pairly's package and its uids, one per phone
/// profile it's installed in (the release app first).
fn pairly_uids(list: &str) -> Option<(&'static str, String)> {
    PACKAGES.iter().find_map(|&name| {
        list.lines().find_map(|l| {
            let mut f = l.split_whitespace();
            let package = f.next()?.strip_prefix("package:")?;
            let uids = f.next()?.strip_prefix("uid:")?;
            let valid = !uids.is_empty() && uids.split(',').all(|u| u.parse::<u32>().is_ok());
            (package == name && valid).then(|| (name, uids.to_owned()))
        })
    })
}

/// Start the helper on every plugged-in phone that has Pairly, replacing one already running
/// there (it may be from an older Pairly). `command` makes an `adb` command. Returns how many
/// started.
pub async fn start_helpers(command: impl Fn() -> Command + Sync) -> usize {
    let command: &(dyn Fn() -> Command + Sync) = &command;
    let Some(list) = adb(command, &["devices"]).await else {
        return 0; // no adb
    };
    let mut started = 0;
    for serial in ready_devices(&list) {
        let s = serial.as_str();
        let Some(packages) = adb(
            command,
            &[
                "-s",
                s,
                "shell",
                "pm",
                "list",
                "packages",
                "-U",
                "io.github.abhilesh1412.pairly",
            ],
        )
        .await
        else {
            continue;
        };
        let Some((package, uids)) = pairly_uids(&packages) else {
            debug!(serial = s, "no Pairly on this phone");
            continue;
        };
        let Some(path) = adb(command, &["-s", s, "shell", "pm", "path", package]).await else {
            continue;
        };
        let Some(apk) = path
            .lines()
            .filter_map(|l| l.trim().strip_prefix("package:"))
            .find(|p| p.ends_with("/base.apk"))
        else {
            continue;
        };
        // Detached (setsid), so it keeps running after the cable comes out.
        let script = format!(
            "pm grant {package} android.permission.WRITE_SECURE_SETTINGS; \
             {STOP_HELPER} sleep 0.3; \
             CLASSPATH={apk} setsid app_process / {HELPER} --uid {uids} </dev/null >/dev/null 2>&1 &"
        );
        if adb(command, &["-s", s, "shell", &script]).await.is_some() {
            info!(
                serial = s,
                package, "started the screen helper on the phone"
            );
            started += 1;
        }
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_adb_lists() {
        let devices = "List of devices attached\nZA222NPS9W\tdevice\nemulator-5554\toffline\nX1\tunauthorized\n\n";
        assert_eq!(ready_devices(devices), ["ZA222NPS9W"]);
        let packages = "package:io.github.abhilesh1412.pairly.dev uid:10400\npackage:io.github.abhilesh1412.pairly uid:10482,1210482\n";
        assert_eq!(
            pairly_uids(packages),
            Some(("io.github.abhilesh1412.pairly", "10482,1210482".to_owned()))
        );
        assert_eq!(
            pairly_uids("package:io.github.abhilesh1412.pairly.dev uid:10400"),
            Some(("io.github.abhilesh1412.pairly.dev", "10400".to_owned()))
        );
        assert_eq!(
            pairly_uids("package:io.github.abhilesh1412.pairly uid:1;rm"),
            None
        );
        assert_eq!(pairly_uids(""), None);
    }
}
