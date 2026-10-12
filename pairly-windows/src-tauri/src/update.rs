//! Updates from Pairly's GitHub releases, through Tauri's updater: it reads `latest.json` from
//! the newest release that has a Windows build, and installs only an installer signed with the
//! project's update key (its public half is built into the app). Checked soon after starting
//! and every few hours; with automatic updates off, the user is told and installs from Settings.
//!
//! A release's Windows build is attached by CI some minutes after the release is published (and
//! not at all if that build fails), so the newest release may have no `latest.json` yet. The
//! updater is given each recent release's copy, newest first (from GitHub's releases feed, which
//! isn't rate-limited like the API), and skips the ones that are missing.

use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;
use tauri::Url;

use crate::backend::{Backend, UpdateView};

const FIRST_CHECK: Duration = Duration::from_secs(90);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

const REPO: &str = "ABHILESH1412/pairly";
/// Releases looked at, newest first.
const RECENT: usize = 5;

fn set(backend: &Backend, state: &str, detail: impl Into<String>) {
    *backend
        .update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = UpdateView {
        state: state.into(),
        detail: detail.into(),
    };
    backend.emit();
}

pub async fn run(app: AppHandle, backend: Arc<Backend>) {
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        check(&app, &backend, false).await;
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// Look for a newer version; install it if automatic updates are on (or `install`).
pub async fn check(app: &AppHandle, backend: &Backend, install: bool) {
    if !cfg!(windows) {
        return set(
            backend,
            "unsupported",
            "Updates are built for the Windows app",
        );
    }
    use tauri_plugin_updater::UpdaterExt;
    set(backend, "checking", "");
    let endpoints = endpoints().await;
    let update = match app
        .updater_builder()
        .endpoints(endpoints)
        .and_then(|b| b.build())
    {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    let update = match update {
        Ok(Some(update)) => update,
        Ok(None) => return set(backend, "up-to-date", ""),
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
            tracing::warn!("no release has a Windows build");
            return set(backend, "failed", "No release has a Windows build yet");
        }
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            return set(backend, "failed", e.to_string());
        }
    };
    let version = update.version.clone();
    if !install && !backend.settings().auto_update {
        backend.notify(
            &format!("Pairly {version} is available"),
            "Open Pairly → Settings to install it.",
        );
        return set(backend, "available", version);
    }
    set(backend, "downloading", version.clone());
    // The installer replaces the app and starts it again.
    match update.download_and_install(|_, _| {}, || {}).await {
        Ok(()) => {
            set(backend, "installed", version);
            app.restart();
        }
        Err(e) => {
            tracing::warn!(error = %e, "update failed");
            set(backend, "failed", e.to_string());
        }
    }
}

/// Where to read `latest.json`: each recent release's own copy, newest first, then the latest
/// release's (all the updater had before; also what's left if the feed can't be read).
async fn endpoints() -> Vec<Url> {
    let tags = match recent_tags().await {
        Ok(tags) => tags,
        Err(e) => {
            tracing::debug!(error = %e, "can't read the releases feed");
            Vec::new()
        }
    };
    tags.iter()
        .map(|tag| format!("https://github.com/{REPO}/releases/download/{tag}/latest.json"))
        .chain([format!(
            "https://github.com/{REPO}/releases/latest/download/latest.json"
        )])
        .filter_map(|url| url.parse().ok())
        .collect()
}

/// The newest releases' tags, from GitHub's releases feed.
async fn recent_tags() -> Result<Vec<String>, reqwest::Error> {
    // The same TLS setup as the updater plugin.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let feed = reqwest::Client::builder()
        .user_agent("pairly-windows")
        .timeout(Duration::from_secs(30))
        .build()?
        .get(format!("https://github.com/{REPO}/releases.atom"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(tags_in(&feed))
}

/// Release tags in the feed, newest first (its entries link to `…/releases/tag/<tag>`).
fn tags_in(feed: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for rest in feed.split("/releases/tag/").skip(1) {
        let tag: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
            .collect();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
        if tags.len() == RECENT {
            break;
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_come_from_the_feed_newest_first() {
        let feed = r#"<feed><id>tag:github.com,2008:https://github.com/x/y/releases</id>
            <link type="text/html" rel="alternate" href="https://github.com/x/y/releases"/>
            <entry><id>tag:github.com,2008:Repository/1/v0.1.7</id>
              <link rel="alternate" type="text/html" href="https://github.com/x/y/releases/tag/v0.1.7"/></entry>
            <entry><link rel="alternate" type="text/html" href="https://github.com/x/y/releases/tag/v0.1.6"/>
              <content>see https://github.com/x/y/releases/tag/v0.1.6 too</content></entry>
            <entry><link href="https://github.com/x/y/releases/tag/v0.1.5"/></entry></feed>"#;
        assert_eq!(tags_in(feed), ["v0.1.7", "v0.1.6", "v0.1.5"]);
        assert!(tags_in("<feed></feed>").is_empty());
        let many: String = (0..9)
            .map(|i| format!(r#"<link href="https://github.com/x/y/releases/tag/v1.{i}"/>"#))
            .collect();
        assert_eq!(tags_in(&many).len(), RECENT);
    }

    /// `cargo test --bin pairly -- --ignored real_releases`: reads GitHub.
    #[tokio::test]
    #[ignore = "needs the internet"]
    async fn real_releases_have_a_windows_build() {
        let urls = endpoints().await;
        assert!(urls.len() > 1, "the feed gave no releases");
        let client = reqwest::Client::new();
        let mut found = false;
        for url in urls {
            let status = client.get(url.clone()).send().await.unwrap().status();
            println!("{status} {url}");
            found |= status.is_success();
        }
        assert!(found, "no release has latest.json");
    }
}
