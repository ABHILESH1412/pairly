//! Updates from Pairly's GitHub releases, through Tauri's updater: it reads `latest.json` from
//! the latest release, and installs only an installer signed with the project's update key (its
//! public half is built into the app). Checked soon after starting and every few hours; with
//! automatic updates off, the user is told and installs from Settings.

use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;

use crate::backend::{Backend, UpdateView};

const FIRST_CHECK: Duration = Duration::from_secs(90);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

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
        return set(backend, "unsupported", "Updates are built for the Windows app");
    }
    use tauri_plugin_updater::UpdaterExt;
    set(backend, "checking", "");
    let update = match app.updater().map(|u| async move { u.check().await }) {
        Ok(check) => check.await,
        Err(e) => Err(e),
    };
    let update = match update {
        Ok(Some(update)) => update,
        Ok(None) => return set(backend, "up-to-date", ""),
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
