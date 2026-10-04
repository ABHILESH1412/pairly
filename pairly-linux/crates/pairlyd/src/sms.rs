//! Text messages on a paired phone: the GTK app reads and sends them through the D-Bus API
//! (`ListConversations`, `ListMessages`, `SendSms`); new ones raise `SmsReceived`.

use pairly_core::PeerInfo;
use pairly_plugins::sms::{Message, SmsHost};
use tokio::sync::mpsc;
use tracing::{debug, info};
use zbus::Connection;
use zbus::object_server::SignalEmitter;

use crate::dbus::DaemonIface;

pub enum SmsEvent {
    /// A new message in this thread.
    Received(PeerInfo, i64),
    /// How sending went.
    Status(PeerInfo, bool, String),
}

pub struct LinuxSms(mpsc::UnboundedSender<SmsEvent>);

impl LinuxSms {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<SmsEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }
}

impl SmsHost for LinuxSms {
    fn received(&self, from: &PeerInfo, message: &Message, _name: Option<&str>) {
        let _ = self
            .0
            .send(SmsEvent::Received(from.clone(), message.thread_id));
    }

    fn status(&self, from: &PeerInfo, ok: bool, detail: &str) {
        let _ = self
            .0
            .send(SmsEvent::Status(from.clone(), ok, detail.to_owned()));
    }
}

/// Turn new messages into `SmsReceived` signals (the phone's own notification is mirrored
/// already, so no extra notification here), and send results into notifications.
pub async fn run(conn: Connection, mut events: mpsc::UnboundedReceiver<SmsEvent>) {
    let Ok(emitter) = SignalEmitter::new(&conn, pairly_dbus::OBJECT_PATH) else {
        return;
    };
    while let Some(event) = events.recv().await {
        match event {
            SmsEvent::Received(from, thread) => {
                if let Err(e) =
                    DaemonIface::sms_received(&emitter, &from.id.to_string(), thread).await
                {
                    debug!(error = %e, "can't emit SmsReceived");
                }
            }
            SmsEvent::Status(from, ok, detail) => {
                info!(device = %from.id, ok, %detail, "message send result");
                let summary = if ok {
                    "Message sent"
                } else {
                    "Couldn't send the message"
                };
                crate::notifications::notify(
                    &conn,
                    summary,
                    &format!("{detail}\nOn {}", from.name),
                    &[],
                    false,
                )
                .await;
            }
        }
    }
}

/// MIME type for a file to send in a picture message.
pub fn mime_for(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "amr" => "audio/amr",
        "vcf" => "text/x-vcard",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}
