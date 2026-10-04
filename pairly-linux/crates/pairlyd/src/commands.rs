//! Commands a paired phone may run on this PC. The user defines them (in the GTK app or
//! `commands.json` in the data directory); the phone only ever sends an id from this list.
//! Each run shows a notification, so nothing happens on the PC unnoticed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use pairly_core::PeerInfo;
use pairly_plugins::command::{CommandHost, CommandInfo, CommandPlugin};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};
use zbus::Connection;

use crate::notifications;

/// Commands that take longer are stopped.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    pub name: String,
    /// Run with `sh -c`.
    pub command: String,
}

pub struct Commands {
    path: PathBuf,
    list: Mutex<Vec<Command>>,
    runs: mpsc::UnboundedSender<(PeerInfo, Command)>,
    plugin: OnceLock<Weak<CommandPlugin>>,
}

impl Commands {
    pub fn load(
        data_dir: &std::path::Path,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<(PeerInfo, Command)>) {
        let path = data_dir.join("commands.json");
        let list = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (runs, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                path,
                list: Mutex::new(list),
                runs,
                plugin: OnceLock::new(),
            }),
            rx,
        )
    }

    pub fn set_plugin(&self, plugin: &Arc<CommandPlugin>) {
        let _ = self.plugin.set(Arc::downgrade(plugin));
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Command>> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn list(&self) -> Vec<Command> {
        self.lock().clone()
    }

    fn save_and_announce(&self, list: &[Command]) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, json)?;
        if let Some(plugin) = self.plugin.get().and_then(Weak::upgrade) {
            plugin.commands_changed();
        }
        Ok(())
    }

    pub fn add(&self, name: &str, command: &str) -> std::io::Result<String> {
        let (name, command) = (name.trim(), command.trim());
        if name.is_empty() || command.is_empty() {
            return Err(std::io::Error::other(
                "a command needs a name and a command line",
            ));
        }
        let id = format!("{:016x}", rand_id());
        let list = {
            let mut list = self.lock();
            list.push(Command {
                id: id.clone(),
                name: name.to_owned(),
                command: command.to_owned(),
            });
            list.clone()
        };
        self.save_and_announce(&list)?;
        Ok(id)
    }

    pub fn remove(&self, id: &str) -> std::io::Result<bool> {
        let (removed, list) = {
            let mut list = self.lock();
            let before = list.len();
            list.retain(|c| c.id != id);
            (list.len() != before, list.clone())
        };
        if removed {
            self.save_and_announce(&list)?;
        }
        Ok(removed)
    }
}

fn rand_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish()
}

impl CommandHost for Commands {
    fn commands(&self) -> Vec<CommandInfo> {
        self.lock()
            .iter()
            .map(|c| CommandInfo {
                id: c.id.clone(),
                name: c.name.clone(),
            })
            .collect()
    }

    fn run(&self, from: &PeerInfo, id: &str) {
        let command = self.lock().iter().find(|c| c.id == id).cloned();
        if let Some(command) = command {
            let _ = self.runs.send((from.clone(), command));
        }
    }
}

/// Run requested commands one at a time and report back.
pub async fn run(
    conn: Connection,
    commands: Arc<Commands>,
    mut runs: mpsc::UnboundedReceiver<(PeerInfo, Command)>,
) {
    while let Some((from, command)) = runs.recv().await {
        info!(device = %from.id, name = %command.name, "running a command for a paired device");
        notifications::show_simple(
            &conn,
            format!("{} ran “{}”", from.name, command.name),
            command.command.clone(),
        );
        let (success, message) = execute(&command.command).await;
        if !success {
            warn!(name = %command.name, %message, "command failed");
        }
        if let Some(plugin) = commands.plugin.get().and_then(Weak::upgrade) {
            plugin.finished(from.id, &command.id, success, &message);
        }
    }
}

async fn execute(line: &str) -> (bool, String) {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(RUN_TIMEOUT, child).await {
        Err(_) => (false, "stopped after 60 seconds".into()),
        Ok(Err(e)) => (false, format!("couldn't start: {e}")),
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text = text.trim().to_owned();
            let ok = out.status.success();
            let message = if text.is_empty() {
                if ok {
                    "done".to_owned()
                } else {
                    format!("exit status {}", out.status.code().unwrap_or(-1))
                }
            } else {
                text
            };
            (ok, message)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_remove_persist_and_run() {
        let dir = tempfile::tempdir().unwrap();
        let (cmds, _rx) = Commands::load(dir.path());
        let id = cmds.add("Say hi", "echo hi").unwrap();
        assert!(cmds.add(" ", "x").is_err());
        let (again, _rx) = Commands::load(dir.path());
        assert_eq!(again.list()[0].name, "Say hi");
        assert_eq!(cmds.commands()[0].id, id);
        assert!(cmds.remove(&id).unwrap());
        assert!(cmds.list().is_empty());
        assert_eq!(execute("echo hi").await, (true, "hi".to_owned()));
        assert!(!execute("exit 3").await.0);
    }
}
