//! Server side of `dev.pairly.Daemon1` (see the `pairly-dbus` crate for the client proxy).

use pairly_core::{DeviceId, PairlyNode, TransportKind};
use pairly_dbus::Device;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

pub struct DaemonIface {
    pub node: PairlyNode,
    pub features: crate::Features,
    pub qr_timeout: std::time::Duration,
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

#[interface(name = "dev.pairly.Daemon1")]
impl DaemonIface {
    async fn get_identity(&self) -> (String, String) {
        (
            self.node.device_id().to_string(),
            self.node.name().to_owned(),
        )
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

    async fn ping(&self, id: &str, message: &str) -> fdo::Result<()> {
        let message = (!message.is_empty()).then(|| message.to_owned());
        let packet = pairly_plugins::ping::packet(message).map_err(failed)?;
        self.node.send(parse_id(id)?, packet).map_err(failed)?;
        Ok(())
    }

    #[zbus(signal)]
    pub async fn device_changed(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;

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
