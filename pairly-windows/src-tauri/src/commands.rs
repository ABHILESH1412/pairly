//! Commands a paired phone may run on this PC. The user defines them in the window (kept as
//! `commands.json` in the data folder); the phone only ever sends an id from this list. Each
//! run shows a notification, so nothing happens on the PC unnoticed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use pairly_core::PeerInfo;
use pairly_plugins::command::{CommandHost, CommandInfo, CommandPlugin};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Commands that take longer are stopped.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    pub name: String,
    /// Run with `cmd /C`.
    pub command: String,
}

pub struct Commands {
    path: PathBuf,
    list: Mutex<Vec<Command>>,
    runs: mpsc::UnboundedSender<(PeerInfo, Command)>,
    plugin: Mutex<Option<Weak<CommandPlugin>>>,
}

impl Commands {
    pub fn load(data_dir: &Path) -> (Arc<Self>, mpsc::UnboundedReceiver<(PeerInfo, Command)>) {
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
                plugin: Mutex::new(None),
            }),
            rx,
        )
    }

    /// The running node's plugin (a new one each time Pairly is switched on).
    pub fn set_plugin(&self, plugin: Option<&Arc<CommandPlugin>>) {
        *self.plugin.lock().unwrap_or_else(PoisonError::into_inner) = plugin.map(Arc::downgrade);
    }

    fn plugin(&self) -> Option<Arc<CommandPlugin>> {
        self.plugin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
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
        if let Some(plugin) = self.plugin() {
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
        if self.lock().len() >= pairly_plugins::command::MAX_COMMANDS {
            return Err(std::io::Error::other(
                "that's as many commands as Pairly keeps",
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

    /// Tell the phone how a run went.
    pub fn finished(&self, from: &PeerInfo, id: &str, success: bool, message: &str) {
        if let Some(plugin) = self.plugin() {
            plugin.finished(from.id, id, success, message);
        }
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

/// The plugin's view: the list, and runs queued for [`run_queue`].
pub struct CommandsHost(pub Arc<Commands>);

impl CommandHost for CommandsHost {
    fn commands(&self) -> Vec<CommandInfo> {
        self.0
            .lock()
            .iter()
            .map(|c| CommandInfo {
                id: c.id.clone(),
                name: c.name.clone(),
            })
            .collect()
    }

    fn run(&self, from: &PeerInfo, id: &str) {
        let command = self.0.lock().iter().find(|c| c.id == id).cloned();
        if let Some(command) = command {
            let _ = self.0.runs.send((from.clone(), command));
        }
    }
}

/// Run requested commands one at a time; `notify` tells the user, then the phone hears back.
pub async fn run_queue(
    commands: Arc<Commands>,
    mut runs: mpsc::UnboundedReceiver<(PeerInfo, Command)>,
    notify: impl Fn(&str, &str),
) {
    while let Some((from, command)) = runs.recv().await {
        tracing::info!(device = %from.id, name = %command.name, "running a command for a paired device");
        notify(
            &format!("{} ran “{}”", from.name, command.name),
            &command.command,
        );
        let (success, message) = execute(&command.command).await;
        if !success {
            tracing::warn!(name = %command.name, %message, "command failed");
        }
        commands.finished(&from, &command.id, success, &message);
    }
}

pub async fn execute(line: &str) -> (bool, String) {
    #[cfg(windows)]
    let child = {
        // No console window flashing up.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").raw_arg(line).creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(not(windows))]
    let child = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line);
        c
    };
    let mut child = child;
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_else(|| ".".into());
    let output = child
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(RUN_TIMEOUT, output).await {
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
        let dir = std::env::temp_dir().join(format!("pairly-cmd-test-{}", rand_id()));
        let (cmds, _rx) = Commands::load(&dir);
        let id = cmds.add("Say hi", "echo hi").unwrap();
        assert!(cmds.add(" ", "x").is_err());
        let (again, _rx) = Commands::load(&dir);
        assert_eq!(again.list()[0].name, "Say hi");
        assert_eq!(CommandsHost(cmds.clone()).commands()[0].id, id);
        assert!(cmds.remove(&id).unwrap());
        assert!(cmds.list().is_empty());
        assert_eq!(execute("echo hi").await, (true, "hi".to_owned()));
        assert!(!execute("exit 3").await.0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
