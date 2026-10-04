//! Calls on a paired phone: a notification while it rings (Answer / Speaker / Reject), one
//! during the call (Hang up), one for a missed call; and this PC's media paused while a call
//! rings or is active (resumed afterwards).

use std::sync::{Arc, OnceLock, Weak};

use futures_util::StreamExt;
use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::telephony::{CallAction, CallEvent, CallState, TelephonyHost, TelephonyPlugin};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::Connection;

use crate::notifications;

pub struct LinuxTelephony {
    events: mpsc::UnboundedSender<(PeerInfo, CallEvent)>,
    plugin: OnceLock<Weak<TelephonyPlugin>>,
}

impl LinuxTelephony {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<(PeerInfo, CallEvent)>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events,
                plugin: OnceLock::new(),
            }),
            rx,
        )
    }

    pub fn set_plugin(&self, plugin: &Arc<TelephonyPlugin>) {
        let _ = self.plugin.set(Arc::downgrade(plugin));
    }

    fn plugin(&self) -> Option<Arc<TelephonyPlugin>> {
        self.plugin.get().and_then(Weak::upgrade)
    }
}

impl TelephonyHost for LinuxTelephony {
    fn call(&self, from: &PeerInfo, event: &CallEvent) {
        let _ = self.events.send((from.clone(), event.clone()));
    }
}

/// The call notification currently showing, and whose phone it is about.
struct Shown {
    id: u32,
    peer: DeviceId,
}

pub async fn run(
    conn: Connection,
    telephony: Arc<LinuxTelephony>,
    mut events: mpsc::UnboundedReceiver<(PeerInfo, CallEvent)>,
    pause_media: bool,
) {
    let mut actions = match zbus::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    {
        Ok(p) => p.receive_signal("ActionInvoked").await.ok(),
        Err(_) => None,
    };
    // Players we paused, to resume when the call is over.
    let mut paused: Vec<String> = Vec::new();
    let mut shown: Option<Shown> = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some((from, event)) = event else { return };
                on_call(&conn, &from, &event, &mut shown, &mut paused, pause_media).await;
            }
            Some(m) = async {
                match actions.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let Ok((id, key)) = m.body().deserialize::<(u32, String)>() else { continue };
                let Some(s) = shown.as_ref().filter(|s| s.id == id) else { continue };
                let action = match key.as_str() {
                    "answer" => CallAction::Answer,
                    "speaker" => CallAction::AnswerOnSpeaker,
                    "reject" => CallAction::Reject,
                    "hangup" => CallAction::HangUp,
                    _ => continue,
                };
                info!(?action, "call action from this PC");
                match telephony.plugin().map(|p| p.control(s.peer, action)) {
                    Some(Err(e)) => warn!(error = %e, "can't reach the phone"),
                    None => debug!("telephony plugin gone"),
                    Some(Ok(())) => {}
                }
            }
        }
    }
}

async fn on_call(
    conn: &Connection,
    from: &PeerInfo,
    event: &CallEvent,
    shown: &mut Option<Shown>,
    paused: &mut Vec<String>,
    pause_media: bool,
) {
    let caller = event.caller().to_owned();
    info!(device = %from.id, state = ?event.state, "call");
    if let Some(s) = shown.take() {
        notifications::close(conn, s.id).await;
    }
    let note = match event.state {
        CallState::Ringing => {
            notifications::notify(
                conn,
                &format!("Incoming call from {caller}"),
                &format!("On {}", from.name),
                &[
                    ("answer", "Answer"),
                    ("speaker", "Speaker"),
                    ("reject", "Reject"),
                ],
                true,
            )
            .await
        }
        CallState::Talking => {
            notifications::notify(
                conn,
                &format!("On a call with {caller}"),
                &format!("On {}", from.name),
                &[("hangup", "Hang Up")],
                true,
            )
            .await
        }
        CallState::Missed => {
            notifications::notify(
                conn,
                &format!("Missed call from {caller}"),
                &format!("On {}", from.name),
                &[],
                false,
            )
            .await;
            None
        }
        CallState::Ended => None,
    };
    *shown = note.map(|id| Shown { id, peer: from.id });
    if !pause_media {
        return;
    }
    match event.state {
        CallState::Ringing | CallState::Talking => {
            for name in crate::media::pause_playing(conn).await {
                if !paused.contains(&name) {
                    paused.push(name);
                }
            }
        }
        CallState::Missed | CallState::Ended => {
            crate::media::resume(conn, &std::mem::take(paused)).await;
        }
    }
}
