//! Server side of `io.github.abhilesh1412.Pairly.Daemon1` (see the `pairly-dbus` crate for the client proxy).

use pairly_core::{DeviceId, PairlyNode, TransportKind};
use pairly_dbus::Device;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

pub struct DaemonIface {
    pub node: PairlyNode,
    pub features: crate::Features,
    pub qr_timeout: std::time::Duration,
    pub data_dir: std::path::PathBuf,
    pub cache_dir: std::path::PathBuf,
    pub notification_apps: std::sync::Arc<crate::notification_apps::AppFilter>,
    /// The config file, where a new name is saved.
    pub config_path: std::path::PathBuf,
    /// Restart the daemon (to take a new name).
    pub restart: std::sync::Arc<tokio::sync::Notify>,
    /// Stop the daemon (Pairly turned off).
    pub quit: std::sync::Arc<tokio::sync::Notify>,
    pub updater: std::sync::Arc<crate::update::Updater>,
}

fn parse_id(id: &str) -> fdo::Result<DeviceId> {
    id.parse()
        .map_err(|_| fdo::Error::InvalidArgs(format!("not a device id: {id:?}")))
}

fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

pub fn link_name(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Memory => "memory",
        TransportKind::Lan => "lan",
        TransportKind::Bluetooth => "bluetooth",
        TransportKind::Relay => "relay",
    }
}

#[interface(name = "io.github.abhilesh1412.Pairly.Daemon1")]
impl DaemonIface {
    async fn get_identity(&self) -> (String, String) {
        (
            self.node.device_id().to_string(),
            self.node.name().to_owned(),
        )
    }

    /// The updater: (this version, state, detail, automatic updates on). States: idle,
    /// checking, up-to-date, available, installing, installed, managed (the package
    /// manager's to update), failed; the detail is the new version or the error.
    async fn update_status(&self) -> (String, String, String, bool) {
        let (state, detail) = self.updater.status().describe();
        (
            crate::update::VERSION.to_owned(),
            state.to_owned(),
            detail,
            self.updater.auto(),
        )
    }

    async fn check_for_updates(&self) {
        self.updater.check_now();
    }

    /// Install the update found (when automatic updates are off).
    async fn install_update(&self) {
        self.updater.install_now();
    }

    async fn set_auto_update(&self, on: bool) {
        self.updater.set_auto(on);
    }

    /// Pause a paired device or resume it. Paused, it stays paired but nothing passes either
    /// way: its connection closes and its reconnects are refused at the handshake.
    async fn set_paused(&self, id: &str, paused: bool) -> fdo::Result<()> {
        self.node
            .set_paused(parse_id(id)?, paused)
            .await
            .map_err(failed)
    }

    /// Stop the daemon: every device disconnects until it starts again. The app turns Pairly
    /// off with this when systemd isn't managing the daemon.
    async fn quit(&self) {
        tracing::info!("quit requested over D-Bus");
        self.quit.notify_one();
    }

    /// Rename this PC: saved in the config file, then the daemon restarts to announce it.
    /// Paired devices see the new name when they reconnect (a few seconds).
    async fn set_name(&self, name: &str) -> fdo::Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > pairly_core::Identity::MAX_NAME_CHARS {
            return Err(fdo::Error::InvalidArgs(
                "the name must be 1 to 64 characters".into(),
            ));
        }
        if name.chars().any(char::is_control) {
            return Err(fdo::Error::InvalidArgs(
                "the name can't contain control characters".into(),
            ));
        }
        if name == self.node.name() {
            return Ok(());
        }
        crate::config::save_name(&self.config_path, name).map_err(|e| failed(format!("{e:#}")))?;
        tracing::info!(name, "renamed: restarting");
        self.restart.notify_one();
        Ok(())
    }

    async fn list_devices(&self) -> fdo::Result<Vec<Device>> {
        let devices = self.node.devices().map_err(failed)?;
        Ok(devices
            .into_iter()
            .map(|d| {
                let battery = self.features.battery.peer_state(d.id);
                Device {
                    id: d.id.to_string(),
                    name: d.name,
                    device_type: d
                        .device_type
                        .map(|t| t.as_str().to_owned())
                        .unwrap_or_default(),
                    paired: d.paired,
                    link: d.link.map(link_name).unwrap_or_default().to_owned(),
                    rtt_ms: d.rtt.map_or(0, |r| {
                        u32::try_from(r.as_millis()).unwrap_or(u32::MAX).max(1)
                    }),
                    battery: battery.map_or(-1, |b| i32::from(b.percent)),
                    charging: battery.is_some_and(|b| b.charging),
                    paused: d.paused,
                }
            })
            .collect())
    }

    async fn request_pair(&self, id: &str) -> fdo::Result<()> {
        self.node.request_pair(parse_id(id)?).await.map_err(failed)
    }

    async fn confirm_pair(&self, id: &str, accept: bool) -> fdo::Result<()> {
        self.node
            .confirm_pair(parse_id(id)?, accept)
            .map_err(failed)
    }

    async fn start_qr_pairing(&self) -> fdo::Result<(String, u32)> {
        let invite = self.node.start_qr_pairing().map_err(failed)?;
        let valid = u32::try_from(self.qr_timeout.as_secs()).unwrap_or(u32::MAX);
        Ok((invite.to_uri(), valid))
    }

    async fn cancel_qr_pairing(&self) {
        self.node.cancel_qr_pairing();
    }

    async fn reply_to_notification(
        &self,
        device: &str,
        notification: &str,
        text: &str,
    ) -> fdo::Result<()> {
        let result =
            self.features
                .notifications
                .request_reply(parse_id(device)?, notification, text);
        match &result {
            Ok(()) => tracing::info!(device, "reply sent"),
            Err(e) => tracing::warn!(device, error = %e, "reply failed"),
        }
        result.map_err(failed)
    }

    async fn unpair(&self, id: &str) -> fdo::Result<()> {
        self.node.unpair(parse_id(id)?).map_err(failed)
    }

    async fn ring(&self, id: &str, on: bool) -> fdo::Result<()> {
        self.features.findmy.ring(parse_id(id)?, on).map_err(failed)
    }

    async fn send_clipboard(&self, id: &str) -> fdo::Result<()> {
        let text = crate::clipboard::read()
            .await
            .ok_or_else(|| fdo::Error::Failed("the clipboard has no text".into()))?;
        self.features
            .clipboard
            .send_to(parse_id(id)?, &text)
            .map_err(failed)
    }

    async fn send_files(&self, id: &str, paths: Vec<String>) -> fdo::Result<Vec<u64>> {
        let ids = self
            .features
            .share
            .send_files(parse_id(id)?, &paths)
            .map_err(|e| fdo::Error::Failed(format!("{e:#}")))?;
        tracing::info!(device = id, files = ids.len(), "sending files");
        Ok(ids)
    }

    async fn send_text(&self, id: &str, text: &str, url: bool) -> fdo::Result<()> {
        self.features
            .share
            .plugin
            .send_text(parse_id(id)?, text, url)
            .map_err(failed)
    }

    async fn accept_transfer(&self, transfer: u64) -> fdo::Result<()> {
        self.features
            .share
            .accept(transfer)
            .map_err(|e| fdo::Error::Failed(format!("{e:#}")))
    }

    async fn cancel_transfer(&self, transfer: u64) -> fdo::Result<()> {
        self.features.share.cancel(transfer).map_err(failed)
    }

    async fn list_transfers(&self) -> Vec<pairly_dbus::Transfer> {
        self.features
            .share
            .plugin
            .transfers()
            .iter()
            .map(|t| crate::share::to_dbus(t, None))
            .collect()
    }

    async fn list_conversations(&self, id: &str) -> fdo::Result<Vec<pairly_dbus::Conversation>> {
        let list = self
            .features
            .sms
            .conversations(parse_id(id)?)
            .await
            .map_err(failed)?;
        Ok(list
            .into_iter()
            .map(|c| pairly_dbus::Conversation {
                thread_id: c.thread_id,
                addresses: c.addresses,
                names: c.names,
                snippet: c.snippet,
                date_ms: c.date_ms,
                read: c.read,
            })
            .collect())
    }

    async fn list_messages(
        &self,
        id: &str,
        thread_id: i64,
        before_ms: i64,
    ) -> fdo::Result<Vec<pairly_dbus::TextMessage>> {
        let before = (before_ms > 0).then_some(before_ms);
        let list = self
            .features
            .sms
            .messages(parse_id(id)?, thread_id, before, 50)
            .await
            .map_err(failed)?;
        Ok(list
            .into_iter()
            .map(|m| pairly_dbus::TextMessage {
                id: m.id,
                thread_id: m.thread_id,
                address: m.address,
                body: m.body,
                date_ms: m.date_ms,
                outgoing: m.outgoing,
                participants: m.participants,
                attachments: m
                    .attachments
                    .into_iter()
                    .map(|a| pairly_dbus::MessageAttachment {
                        part_id: a.part_id,
                        mime: a.mime,
                        name: a.name,
                        size: a.size,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn send_sms(&self, id: &str, addresses: Vec<String>, text: &str) -> fdo::Result<()> {
        self.features
            .sms
            .send(parse_id(id)?, addresses, text, Vec::new())
            .map_err(failed)
    }

    async fn send_mms(
        &self,
        id: &str,
        addresses: Vec<String>,
        text: &str,
        files: Vec<String>,
    ) -> fdo::Result<()> {
        let mut attachments = Vec::new();
        for path in &files {
            let path = std::path::Path::new(path);
            let meta = std::fs::metadata(path).map_err(failed)?;
            if meta.len() > u64::from(pairly_plugins::sms::ATTACHMENT_CHUNK) {
                return Err(fdo::Error::Failed(format!(
                    "{} is too big for a picture message (at most {} KB)",
                    path.display(),
                    pairly_plugins::sms::ATTACHMENT_CHUNK / 1024
                )));
            }
            attachments.push(pairly_plugins::sms::OutgoingAttachment {
                mime: crate::sms::mime_for(path).to_owned(),
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                data: std::fs::read(path).map_err(failed)?,
            });
        }
        self.features
            .sms
            .send(parse_id(id)?, addresses, text, attachments)
            .map_err(failed)
    }

    async fn sms_attachment(&self, id: &str, part_id: i64, name: &str) -> fdo::Result<String> {
        let peer = parse_id(id)?;
        let dir = self.cache_dir.join("mms");
        let file = dir.join(format!(
            "{peer}-{part_id}-{}",
            pairly_plugins::share::safe_file_name(name)
        ));
        if !file.exists() {
            let data = self
                .features
                .sms
                .attachment(peer, part_id, 20 * 1024 * 1024)
                .await
                .map_err(failed)?;
            std::fs::create_dir_all(&dir).map_err(failed)?;
            std::fs::write(&file, data).map_err(failed)?;
        }
        Ok(file.display().to_string())
    }

    async fn files_list(&self, id: &str, path: &str) -> fdo::Result<Vec<pairly_dbus::FileEntry>> {
        let list = self
            .features
            .files
            .list(parse_id(id)?, path)
            .await
            .map_err(failed)?;
        Ok(list
            .into_iter()
            .map(|e| pairly_dbus::FileEntry {
                name: e.name,
                dir: e.dir,
                size: e.size,
                modified_ms: e.modified_ms,
            })
            .collect())
    }

    async fn files_download(
        &self,
        id: &str,
        path: &str,
        size: u64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let peer = parse_id(id)?;
        let name = path.rsplit('/').next().unwrap_or("file");
        let name = pairly_plugins::share::safe_file_name(name);
        let dir = self.features.share.download_dir().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(failed)?;
        let target = crate::share::unique_path(&dir, &name);
        let part = target.with_file_name(format!(
            "{}.part",
            target
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ));
        let result = async {
            use std::io::Write;
            let mut out = std::fs::File::create(&part).map_err(failed)?;
            let mut offset = 0u64;
            loop {
                let chunk = self
                    .features
                    .files
                    .read(peer, path, offset, pairly_plugins::files::CHUNK)
                    .await
                    .map_err(failed)?;
                if chunk.is_empty() {
                    break;
                }
                out.write_all(&chunk).map_err(failed)?;
                offset += chunk.len() as u64;
                let _ = Self::files_progress(&emitter, id, path, offset, size.max(offset)).await;
            }
            out.sync_all().map_err(failed)?;
            std::fs::rename(&part, &target).map_err(failed)
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result?;
        tracing::info!(device = id, path, "downloaded a file from the phone");
        Ok(target.display().to_string())
    }

    async fn files_upload(
        &self,
        id: &str,
        local_path: &str,
        remote_dir: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        use pairly_plugins::files::{CHUNK, FileOp};
        use std::os::unix::fs::FileExt;
        let peer = parse_id(id)?;
        let local = std::path::Path::new(local_path);
        let file = std::fs::File::open(local).map_err(failed)?;
        let size = file.metadata().map_err(failed)?.len();
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| fdo::Error::InvalidArgs("not a file".into()))?;
        let remote = if remote_dir.trim_matches('/').is_empty() {
            name
        } else {
            format!("{}/{name}", remote_dir.trim_matches('/'))
        };
        let mut offset = 0u64;
        loop {
            let mut buf = vec![0u8; CHUNK as usize];
            let n = file.read_at(&mut buf, offset).map_err(failed)?;
            buf.truncate(n);
            if n == 0 && offset > 0 {
                break;
            }
            let op = FileOp::Write {
                path: remote.clone(),
                offset,
                data: buf,
                create: offset == 0,
            };
            self.features.files.ask(peer, op).await.map_err(failed)?;
            offset += n as u64;
            let _ = Self::files_progress(&emitter, id, &remote, offset, size).await;
            if n == 0 {
                break;
            }
        }
        Ok(())
    }

    async fn files_delete(&self, id: &str, path: &str) -> fdo::Result<()> {
        let op = pairly_plugins::files::FileOp::Delete {
            path: path.to_owned(),
        };
        self.features
            .files
            .ask(parse_id(id)?, op)
            .await
            .map(drop)
            .map_err(failed)
    }

    async fn files_mkdir(&self, id: &str, path: &str) -> fdo::Result<()> {
        let op = pairly_plugins::files::FileOp::Mkdir {
            path: path.to_owned(),
        };
        self.features
            .files
            .ask(parse_id(id)?, op)
            .await
            .map(drop)
            .map_err(failed)
    }

    async fn files_rename(&self, id: &str, from: &str, to: &str) -> fdo::Result<()> {
        let op = pairly_plugins::files::FileOp::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        };
        self.features
            .files
            .ask(parse_id(id)?, op)
            .await
            .map(drop)
            .map_err(failed)
    }

    /// Open a window showing the phone's screen (it asks its user first).
    async fn show_phone_screen(&self, id: &str) -> fdo::Result<()> {
        let peer = parse_id(id)?;
        let name = self
            .node
            .devices()
            .ok()
            .and_then(|list| list.into_iter().find(|d| d.id == peer))
            .map_or_else(|| "Phone".to_owned(), |d| d.name);
        self.features
            .screen_host
            .open(peer, &name)
            .map_err(fdo::Error::Failed)
    }

    async fn phone_power(&self, id: &str, action: &str) -> fdo::Result<()> {
        use pairly_plugins::power::PowerAction;
        let action = match action {
            "lock" => PowerAction::Lock,
            "poweroff" => PowerAction::PowerOff,
            "restart" => PowerAction::Restart,
            other => return Err(fdo::Error::InvalidArgs(format!("unknown action {other}"))),
        };
        let peer = parse_id(id)?;
        tracing::info!(device = %peer, ?action, "power action");
        self.features
            .power
            .request(peer, action)
            .await
            .map_err(|e| match e {
                // The phone's own explanation, as it gave it.
                pairly_core::CoreError::Transport(why) => fdo::Error::Failed(why),
                other => failed(other),
            })
    }

    async fn list_contacts(&self, id: &str) -> fdo::Result<Vec<pairly_dbus::Contact>> {
        let peer = parse_id(id)?;
        let list = self.features.contacts.fetch(peer).await.map_err(failed)?;
        crate::contacts::save_vcards(&self.data_dir, peer, &list);
        Ok(list
            .into_iter()
            .map(|c| pairly_dbus::Contact {
                name: c.name,
                numbers: c.numbers,
            })
            .collect())
    }

    async fn dial(&self, id: &str, number: &str) -> fdo::Result<()> {
        self.features
            .telephony
            .dial(parse_id(id)?, number)
            .map_err(failed)
    }

    async fn call_action(&self, id: &str, action: &str) -> fdo::Result<()> {
        use pairly_plugins::telephony::CallAction;
        let action = match action {
            "answer" => CallAction::Answer,
            "speaker" => CallAction::AnswerOnSpeaker,
            "reject" => CallAction::Reject,
            "hangup" => CallAction::HangUp,
            other => {
                return Err(fdo::Error::InvalidArgs(format!(
                    "unknown call action {other:?}"
                )));
            }
        };
        self.features
            .telephony
            .control(parse_id(id)?, action)
            .map_err(failed)
    }

    async fn list_notification_apps(&self) -> Vec<pairly_dbus::NotificationApp> {
        self.notification_apps
            .list()
            .into_iter()
            .map(|(name, state)| pairly_dbus::NotificationApp {
                name,
                send: !state.muted,
                last_seen: state.last_seen,
            })
            .collect()
    }

    async fn set_notification_app_send(&self, app: &str, send: bool) {
        tracing::info!(app, send, "notification app setting");
        self.notification_apps.set_muted(app, !send);
    }

    async fn list_commands(&self) -> Vec<pairly_dbus::Command> {
        self.features
            .commands
            .list()
            .into_iter()
            .map(|c| pairly_dbus::Command {
                id: c.id,
                name: c.name,
                command: c.command,
            })
            .collect()
    }

    async fn add_command(&self, name: &str, command: &str) -> fdo::Result<String> {
        self.features.commands.add(name, command).map_err(failed)
    }

    async fn remove_command(&self, id: &str) -> fdo::Result<()> {
        self.features.commands.remove(id).map(drop).map_err(failed)
    }

    async fn list_players(&self, id: &str) -> fdo::Result<Vec<pairly_dbus::Player>> {
        let peer = parse_id(id)?;
        let players = self.features.media.peer_players(peer);
        Ok(self.features.media_host.to_dbus(peer, &players))
    }

    async fn media_control(
        &self,
        id: &str,
        player: &str,
        action: &str,
        value: i64,
    ) -> fdo::Result<()> {
        use pairly_plugins::media::MediaAction;
        let clamp = |v: i64| u64::try_from(v.max(0)).unwrap_or(0);
        let action = match action {
            "play" => MediaAction::Play,
            "pause" => MediaAction::Pause,
            "play_pause" => MediaAction::PlayPause,
            "stop" => MediaAction::Stop,
            "next" => MediaAction::Next,
            "previous" => MediaAction::Previous,
            "seek" => MediaAction::Seek(value),
            "set_position" => MediaAction::SetPosition(clamp(value)),
            "set_volume" => {
                MediaAction::SetVolume(u8::try_from(clamp(value).min(100)).unwrap_or(100))
            }
            other => {
                return Err(fdo::Error::InvalidArgs(format!(
                    "unknown media action {other:?}"
                )));
            }
        };
        self.features
            .media
            .command(parse_id(id)?, player, action)
            .map_err(failed)
    }

    async fn ping(&self, id: &str, message: &str) -> fdo::Result<()> {
        let message = (!message.is_empty()).then(|| message.to_owned());
        let packet = pairly_plugins::ping::packet(message).map_err(failed)?;
        self.node.send(parse_id(id)?, packet).map_err(failed)?;
        Ok(())
    }

    #[zbus(signal)]
    pub async fn device_changed(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn update_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn pairing_requested(
        emitter: &SignalEmitter<'_>,
        id: &str,
        name: &str,
        code: &str,
        incoming: bool,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn pairing_finished(
        emitter: &SignalEmitter<'_>,
        id: &str,
        success: bool,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn files_progress(
        emitter: &SignalEmitter<'_>,
        id: &str,
        path: &str,
        done: u64,
        total: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn sms_received(
        emitter: &SignalEmitter<'_>,
        id: &str,
        thread_id: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn players_changed(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn transfer_changed(
        emitter: &SignalEmitter<'_>,
        transfer: pairly_dbus::Transfer,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn ping_received(
        emitter: &SignalEmitter<'_>,
        id: &str,
        name: &str,
        message: &str,
    ) -> zbus::Result<()>;
}
