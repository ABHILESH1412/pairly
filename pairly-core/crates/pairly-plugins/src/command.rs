//! Run commands: the PC publishes a list of named commands set up by its user; a paired phone
//! can run one by id and hears how it went. The phone never sends a command line, only an id
//! from the list, so it can only run what the PC's user allowed.

use std::sync::Arc;

use async_trait::async_trait;
use pairly_core::{
    DeviceId, Envelope, OutboundPacket, PacketBody, PeerInfo, Plugin, PluginCtx, Priority, Result,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::peers::Peers;

pub const MAX_COMMANDS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}

impl PacketBody for CommandList {
    const TYPE: &'static str = "command.list";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRun {
    pub id: String,
}

impl PacketBody for CommandRun {
    const TYPE: &'static str = "command.run";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDone {
    pub id: String,
    pub success: bool,
    /// The end of the output, or why it failed.
    pub message: String,
}

impl PacketBody for CommandDone {
    const TYPE: &'static str = "command.done";
}

pub trait CommandHost: Send + Sync + 'static {
    /// Commands this device offers (the PC); none by default.
    fn commands(&self) -> Vec<CommandInfo> {
        Vec::new()
    }
    /// Run one of [`CommandHost::commands`] for `from`, then call
    /// [`CommandPlugin::finished`].
    fn run(&self, _from: &PeerInfo, _id: &str) {}
    /// A peer's command list changed.
    fn peer_commands(&self, _from: &PeerInfo, _commands: &[CommandInfo]) {}
    /// A command we asked a peer to run finished.
    fn peer_finished(&self, _from: &PeerInfo, _done: &CommandDone) {}
}

pub struct CommandPlugin {
    host: Arc<dyn CommandHost>,
    peers: Peers,
}

impl CommandPlugin {
    pub fn new(host: Arc<dyn CommandHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            peers: Peers::default(),
        })
    }

    fn list_packet(&self) -> Result<OutboundPacket> {
        let mut commands = self.host.commands();
        commands.truncate(MAX_COMMANDS);
        OutboundPacket::reliable(&CommandList { commands }, Priority::Interactive)
    }

    /// Our commands changed: tell every connected peer.
    pub fn commands_changed(&self) {
        if let Ok(packet) = self.list_packet() {
            self.peers.broadcast(&packet);
        }
    }

    /// Ask a peer to run one of its commands.
    pub fn run(&self, peer: DeviceId, id: &str) -> Result<()> {
        let run = CommandRun { id: id.to_owned() };
        self.peers
            .send(peer, OutboundPacket::reliable(&run, Priority::Interactive)?)
    }

    /// Report a command started by [`CommandHost::run`].
    pub fn finished(&self, peer: DeviceId, id: &str, success: bool, message: &str) {
        let message: String = message
            .chars()
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let done = CommandDone {
            id: id.to_owned(),
            success,
            message,
        };
        if let Ok(packet) = OutboundPacket::reliable(&done, Priority::Interactive) {
            let _ = self.peers.send(peer, packet);
        }
    }
}

#[async_trait]
impl Plugin for CommandPlugin {
    fn id(&self) -> &'static str {
        "command"
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[CommandList::TYPE, CommandRun::TYPE, CommandDone::TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        self.incoming()
    }

    async fn on_connected(&self, ctx: &PluginCtx) {
        self.peers.insert(ctx);
        if let Ok(packet) = self.list_packet() {
            let _ = ctx.send(packet);
        }
    }

    async fn on_packet(&self, ctx: &PluginCtx, packet: Envelope) {
        let from = ctx.peer_info();
        match packet.ty.as_str() {
            CommandList::TYPE => match packet.body::<CommandList>() {
                Ok(mut list) => {
                    list.commands.truncate(MAX_COMMANDS);
                    self.host.peer_commands(&from, &list.commands);
                }
                Err(e) => debug!(peer = %from.id, error = %e, "bad command.list"),
            },
            CommandRun::TYPE => match packet.body::<CommandRun>() {
                // Only ids we published; anything else is refused.
                Ok(run) if self.host.commands().iter().any(|c| c.id == run.id) => {
                    self.host.run(&from, &run.id);
                }
                Ok(run) => self.finished(from.id, &run.id, false, "no such command on this PC"),
                Err(e) => debug!(peer = %from.id, error = %e, "bad command.run"),
            },
            CommandDone::TYPE => match packet.body::<CommandDone>() {
                Ok(done) => self.host.peer_finished(&from, &done),
                Err(e) => debug!(peer = %from.id, error = %e, "bad command.done"),
            },
            _ => {}
        }
    }

    async fn on_disconnected(&self, peer: DeviceId) {
        self.peers.remove(peer);
    }
}
