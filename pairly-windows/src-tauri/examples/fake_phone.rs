//! A pretend Android phone, for trying the Windows app without one: a Pairly node that says
//! it's a phone, with a few texts, contacts, files and a music player, a moving test picture
//! as its screen, and commands on stdin to make it do what a phone does (ring the PC, post a
//! notification, take a call, point a laser…). It accepts every pairing on its side.
//!
//! ```sh
//! cargo run --example fake_phone -- <data folder>
//! ```
//! Then type `help`. With `FAKE_PHONE_AS_PC=1` it says it's a desktop PC instead (for trying
//! another PC's screen: the mouse and keyboard it receives are printed).

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pairly_core::registry::Registry;
use pairly_core::{DeviceId, DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo, Platform};
use pairly_crypto::FileKeyStore;
use pairly_plugins::battery::{BatteryHost, BatteryPlugin, BatteryState};
use pairly_plugins::clipboard::{ClipboardHost, ClipboardPlugin};
use pairly_plugins::command::{CommandDone, CommandHost, CommandInfo, CommandPlugin};
use pairly_plugins::contacts::{Contact, ContactsHost, ContactsPlugin};
use pairly_plugins::files::{FilesHost, FilesPlugin};
use pairly_plugins::findmy::{FindMyHost, FindMyPlugin};
use pairly_plugins::input::{
    InputHost, InputPlugin, KeyInput, LaserAction, LaserPointer, Modifiers, PointerButton,
    PointerMotion,
};
use pairly_plugins::media::{MediaAction, MediaHost, MediaPlugin, PlayerState};
use pairly_plugins::notification::{
    Icon, Notification, NotificationAction, NotificationHost, NotificationPlugin,
};
use pairly_plugins::ping::PingPlugin;
use pairly_plugins::power::{PowerAction, PowerHost, PowerPlugin};
use pairly_plugins::screen::{ScreenAction, ScreenFrame, ScreenHost, ScreenInput, ScreenPlugin};
use pairly_plugins::share::{ShareHost, SharePlugin, Transfer};
use pairly_plugins::sms::{
    Attachment, Conversation, Message, OutgoingAttachment, SmsHost, SmsPlugin,
};
use pairly_plugins::telephony::{CallAction, CallEvent, CallState, TelephonyHost, TelephonyPlugin};
use pairly_transport_lan::{LanConfig, LanTransport};

fn say(s: impl AsRef<str>) {
    println!("[phone] {}", s.as_ref());
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A tiny PNG (4×4, purple), for artwork, icons and a picture message.
fn png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 4, 4);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().expect("png header");
        let px: Vec<u8> = (0..16).flat_map(|_| [0x53, 0x4A, 0xB7, 0xFF]).collect();
        w.write_image_data(&px).expect("png data");
    }
    out
}

struct Phone {
    storage: PathBuf,
    received: PathBuf,
    player: Mutex<PlayerState>,
    media: OnceLock<Arc<MediaPlugin>>,
    sms: OnceLock<Arc<SmsPlugin>>,
    screen: OnceLock<Arc<ScreenPlugin>>,
    share: OnceLock<Arc<SharePlugin>>,
    sharing: Arc<AtomicBool>,
    pc_commands: Mutex<Vec<CommandInfo>>,
}

impl Platform for Phone {
    fn ping_received(&self, from: &PeerInfo, message: Option<&str>) {
        say(format!("ping from {}: {:?}", from.name, message));
    }
}

impl ClipboardHost for Phone {
    fn set_clipboard(&self, from: &PeerInfo, text: &str) {
        say(format!("clipboard from {}: {text:?}", from.name));
    }
}

impl BatteryHost for Phone {
    fn current(&self) -> Option<BatteryState> {
        Some(BatteryState {
            percent: 64,
            charging: false,
        })
    }

    fn peer_changed(&self, from: &PeerInfo, state: BatteryState, _previous: Option<BatteryState>) {
        say(format!("{} battery {}%", from.name, state.percent));
    }
}

impl FindMyHost for Phone {
    fn ring(&self, from: &PeerInfo, on: bool) {
        say(format!("RING from {}: {on}", from.name));
    }
}

impl NotificationHost for Phone {
    fn show(&self, from: &PeerInfo, n: &Notification) {
        say(format!(
            "PC notification from {}: [{}] {} | {}",
            from.name, n.app, n.title, n.text
        ));
    }
    fn remove(&self, _from: &PeerInfo, id: &str) {
        say(format!("PC notification removed: {id}"));
    }
    fn sync(&self, _from: &PeerInfo, _active: &[String]) {}
    fn dismiss_local(&self, id: &str) {
        say(format!("PC DISMISSED notification {id}"));
    }
    fn action_local(&self, id: &str, action: &str) {
        say(format!("PC ACTION {action} on notification {id}"));
    }
    fn reply_local(&self, id: &str, text: &str) {
        say(format!("PC REPLY to notification {id}: {text:?}"));
    }
}

impl ShareHost for Phone {
    fn file_offered(&self, t: &Transfer) {
        say(format!("file offered: {} ({} bytes)", t.name, t.size));
        let _ = std::fs::create_dir_all(&self.received);
        if let (Some(share), Ok(file)) = (
            self.share.get(),
            std::fs::File::create(
                self.received
                    .join(pairly_plugins::share::safe_file_name(&t.name)),
            ),
        ) {
            let _ = share.accept(t.id, file);
        }
    }
    fn transfer_changed(&self, t: &Transfer) {
        if t.state.is_finished() {
            say(format!("transfer {} {:?}", t.name, t.state));
        }
    }
    fn text_received(&self, from: &PeerInfo, text: &str, url: bool) {
        say(format!("text from {} (url {url}): {text:?}", from.name));
    }
}

impl PowerHost for Phone {
    fn act(&self, from: &PeerInfo, action: PowerAction) -> Result<(), String> {
        say(format!("POWER {action:?} asked by {}", from.name));
        Ok(())
    }
}

impl InputHost for Phone {
    fn pointer(&self, _: &PeerInfo, _: PointerMotion) {}
    fn button(&self, _: &PeerInfo, _: PointerButton) {}
    fn key(&self, _: &PeerInfo, _: &KeyInput) {}
}

impl MediaHost for Phone {
    fn players(&self) -> Vec<PlayerState> {
        vec![self.player.lock().expect("player").clone()]
    }
    fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        (key == "fake-art").then(png)
    }
    fn peer_players(&self, from: &PeerInfo, players: &[PlayerState]) {
        for p in players {
            say(format!(
                "{} player {} ({}): {} - {} playing={} pos={:?}/{:?} art={:?}",
                from.name,
                p.name,
                p.id,
                p.title,
                p.artist,
                p.playing,
                p.position_ms,
                p.length_ms,
                p.art
            ));
        }
        if players.is_empty() {
            say(format!("{} has no players", from.name));
        }
    }
    fn peer_artwork(&self, from: &PeerInfo, key: &str, data: &[u8]) {
        say(format!(
            "artwork from {}: {key} ({} bytes)",
            from.name,
            data.len()
        ));
    }
    fn command(&self, from: &PeerInfo, player: &str, action: MediaAction) {
        say(format!("MEDIA {action:?} on {player} from {}", from.name));
        {
            let mut p = self.player.lock().expect("player");
            match action {
                MediaAction::PlayPause => p.playing = !p.playing,
                MediaAction::Play => p.playing = true,
                MediaAction::Pause => p.playing = false,
                MediaAction::Next => p.title = format!("{} (next)", p.title),
                MediaAction::SetPosition(ms) => p.position_ms = Some(ms),
                MediaAction::SetVolume(v) => p.volume = Some(v),
                _ => {}
            }
        }
        if let Some(m) = self.media.get() {
            m.local_changed();
        }
    }
}

impl TelephonyHost for Phone {
    fn control(&self, from: &PeerInfo, action: CallAction) {
        say(format!("CALL ACTION {action:?} from {}", from.name));
    }
    fn dial(&self, from: &PeerInfo, number: &str) {
        say(format!("DIAL {number} asked by {}", from.name));
    }
}

impl ContactsHost for Phone {
    fn contacts(&self) -> Result<Vec<Contact>, String> {
        Ok(vec![
            Contact {
                name: "Asha Verma".into(),
                numbers: vec!["+91 98765 43210".into()],
            },
            Contact {
                name: "Ben Ortiz".into(),
                numbers: vec!["+1 415 555 0101".into(), "+1 415 555 0199".into()],
            },
            Contact {
                name: "Chen Li".into(),
                numbers: vec!["+44 20 7946 0958".into()],
            },
        ])
    }
}

impl FilesHost for Phone {
    fn root(&self) -> Result<PathBuf, String> {
        Ok(self.storage.clone())
    }
}

fn conversations() -> Vec<Conversation> {
    let now = now_ms();
    vec![
        Conversation {
            thread_id: 1,
            addresses: vec!["+919876543210".into()],
            names: vec!["Asha Verma".into()],
            snippet: "See you at 6!".into(),
            date_ms: now - 60_000,
            read: false,
        },
        Conversation {
            thread_id: 2,
            addresses: vec!["+14155550101".into(), "+442079460958".into()],
            names: vec!["Ben Ortiz".into(), "Chen Li".into()],
            snippet: "Photo".into(),
            date_ms: now - 86_400_000,
            read: true,
        },
    ]
}

fn messages(thread: i64) -> Vec<Message> {
    let now = now_ms();
    let m = |id: i64,
             address: &str,
             body: &str,
             ago: i64,
             outgoing: bool,
             attachments: Vec<Attachment>| Message {
        id,
        thread_id: thread,
        address: address.into(),
        body: body.into(),
        date_ms: now - ago,
        outgoing,
        read: true,
        participants: Vec::new(),
        attachments,
    };
    match thread {
        1 => vec![
            m(
                1,
                "+919876543210",
                "Hi! Are we still on for today?",
                3_600_000,
                false,
                vec![],
            ),
            m(
                2,
                "+919876543210",
                "Yes, 6 pm works",
                3_000_000,
                true,
                vec![],
            ),
            m(3, "+919876543210", "See you at 6!", 60_000, false, vec![]),
        ],
        2 => vec![
            m(4, "+14155550101", "Look at this", 90_000_000, false, vec![]),
            m(
                5,
                "+442079460958",
                "",
                86_400_000,
                false,
                vec![Attachment {
                    part_id: 77,
                    mime: "image/png".into(),
                    name: "photo.png".into(),
                    size: png().len() as u64,
                }],
            ),
        ],
        _ => Vec::new(),
    }
}

impl SmsHost for Phone {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        say("PC asked for conversations");
        Ok(conversations())
    }
    fn messages(
        &self,
        thread_id: i64,
        _before: Option<i64>,
        _limit: u32,
    ) -> Result<Vec<Message>, String> {
        say(format!("PC asked for thread {thread_id}"));
        Ok(messages(thread_id))
    }
    fn attachment(&self, part_id: i64, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        if part_id != 77 {
            return Err("no such attachment".into());
        }
        let data = png();
        let start = (offset as usize).min(data.len());
        let end = (start + len as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }
    fn send(
        &self,
        addresses: &[String],
        text: &str,
        attachments: &[OutgoingAttachment],
    ) -> Result<(), String> {
        say(format!(
            "SEND SMS to {addresses:?}: {text:?} with {} attachment(s)",
            attachments.len()
        ));
        if let Some(sms) = self.sms.get() {
            let sms = sms.clone();
            let (addresses, text) = (addresses.to_vec(), text.to_owned());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                sms.report(true, "Sent");
                sms.new_message(
                    Message {
                        id: now_ms(),
                        thread_id: if addresses.len() == 1 { 1 } else { 2 },
                        address: addresses.first().cloned().unwrap_or_default(),
                        body: text,
                        date_ms: now_ms(),
                        outgoing: true,
                        read: true,
                        participants: Vec::new(),
                        attachments: Vec::new(),
                    },
                    None,
                );
            });
        }
        Ok(())
    }
}

impl CommandHost for Phone {
    fn peer_commands(&self, from: &PeerInfo, commands: &[CommandInfo]) {
        say(format!(
            "{} commands: {:?}",
            from.name,
            commands
                .iter()
                .map(|c| (&c.id, &c.name))
                .collect::<Vec<_>>()
        ));
        *self.pc_commands.lock().expect("commands") = commands.to_vec();
    }
    fn peer_finished(&self, from: &PeerInfo, done: &CommandDone) {
        say(format!(
            "{} ran {}: success={} {:?}",
            from.name, done.id, done.success, done.message
        ));
    }
}

impl ScreenHost for Phone {
    // Sharing "our" screen: a moving test picture.
    fn start_sharing(&self, from: &PeerInfo) -> Result<(), String> {
        say(format!("{} wants to see this screen: sharing", from.name));
        let Some(screen) = self.screen.get().cloned() else {
            return Err("not ready".into());
        };
        let (peer, sharing) = (from.id, self.sharing.clone());
        sharing.store(true, Ordering::SeqCst);
        std::thread::spawn(move || stream_test_picture(&screen, peer, &sharing));
        Ok(())
    }
    fn stop_sharing(&self, from: &PeerInfo) {
        say(format!("{} stopped watching", from.name));
        self.sharing.store(false, Ordering::SeqCst);
    }
    fn input(&self, from: &PeerInfo, input: ScreenInput) {
        say(format!("SCREEN INPUT from {}: {input:?}", from.name));
    }
    // Watching the PC's screen.
    fn started(&self, from: &PeerInfo, width: u32, height: u32) {
        say(format!("{} screen started: {width}x{height}", from.name));
    }
    fn frame(&self, _from: &PeerInfo, frame: ScreenFrame) {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        // Kept for inspecting what the PC sends (play it with any H.264 player).
        if let Some(dir) = std::env::var_os("FAKE_PHONE_SAVE_SCREEN") {
            use std::io::Write;
            let path = std::path::Path::new(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path.join("pc-screen.h264")) {
                let _ = f.write_all(&frame.data);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path.join("pc-screen.txt")) {
                let _ = writeln!(f, "{} {} {}", now_ms(), frame.data.len(), frame.key);
            }
        }
        if n.is_multiple_of(30) {
            say(format!(
                "PC screen frame #{n}: {} bytes key={}",
                frame.data.len(),
                frame.key
            ));
        }
    }
    fn stopped(&self, from: &PeerInfo, reason: &str) {
        say(format!("{} screen stopped: {reason}", from.name));
    }
}

/// A 360×640 test picture with a box going round, 20 frames a second, until stopped.
fn stream_test_picture(screen: &ScreenPlugin, peer: DeviceId, sharing: &AtomicBool) {
    use openh264::encoder::{Encoder, EncoderConfig, FrameType};
    use openh264::formats::{RgbSliceU8, YUVBuffer};
    let (w, h) = (360usize, 640usize);
    let Ok(mut enc) =
        Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
    else {
        say("no encoder");
        return;
    };
    if let Err(e) = screen.started(peer, w as u32, h as u32) {
        say(format!("can't start: {e}"));
        return;
    }
    let mut rgb = vec![0u8; w * h * 3];
    let mut n = 0usize;
    while sharing.load(Ordering::SeqCst) {
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 3;
                rgb[o] = (x * 255 / w) as u8;
                rgb[o + 1] = (y * 255 / h) as u8;
                rgb[o + 2] = 120;
            }
        }
        let (bx, by) = ((n * 7) % (w - 60), (n * 11) % (h - 60));
        for y in by..by + 60 {
            for x in bx..bx + 60 {
                let o = (y * w + x) * 3;
                rgb[o..o + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
        if n.is_multiple_of(60) {
            enc.force_intra_frame();
        }
        match enc.encode(&yuv) {
            Ok(stream) => {
                let key = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
                let data = stream.to_vec();
                if !data.is_empty() {
                    let behind = screen
                        .frame(
                            peer,
                            &ScreenFrame {
                                data,
                                key,
                                config: false,
                            },
                        )
                        .unwrap_or(false);
                    if behind {
                        enc.force_intra_frame();
                    }
                }
            }
            Err(e) => say(format!("encode: {e}")),
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = screen.stopped(peer, "the phone stopped sharing");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PAIRLY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "fake-phone".into()),
    );
    std::fs::create_dir_all(&dir)?;
    let storage = dir.join("storage");
    std::fs::create_dir_all(storage.join("DCIM/Camera"))?;
    std::fs::create_dir_all(storage.join("Download"))?;
    std::fs::write(storage.join("DCIM/Camera/IMG_0001.png"), png())?;
    std::fs::write(
        storage.join("Download/notes.txt"),
        b"Hello from the fake phone\n",
    )?;
    std::fs::write(storage.join("readme.txt"), vec![b'x'; 1_500_000])?;

    let phone = Arc::new(Phone {
        storage,
        received: dir.join("received"),
        player: Mutex::new(PlayerState {
            id: "com.example.music".into(),
            name: "Music".into(),
            title: "Fake Song".into(),
            artist: "The Testers".into(),
            album: "Unit Tests".into(),
            art: Some("fake-art".into()),
            length_ms: Some(215_000),
            position_ms: Some(42_000),
            playing: true,
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            can_seek: true,
            volume: Some(70),
        }),
        media: OnceLock::new(),
        sms: OnceLock::new(),
        screen: OnceLock::new(),
        share: OnceLock::new(),
        sharing: Arc::new(AtomicBool::new(false)),
        pc_commands: Mutex::new(Vec::new()),
    });
    let notifications = NotificationPlugin::new(phone.clone());
    let media = MediaPlugin::new(phone.clone());
    let sms = SmsPlugin::new(phone.clone());
    let screen = ScreenPlugin::new(phone.clone());
    let share = SharePlugin::new(phone.clone());
    let telephony = TelephonyPlugin::new(phone.clone());
    let findmy = FindMyPlugin::new(phone.clone());
    let clipboard = ClipboardPlugin::new(phone.clone());
    let command = CommandPlugin::new(phone.clone());
    let input = InputPlugin::new(phone.clone());
    let power = PowerPlugin::new(phone.clone());
    let _ = phone.media.set(media.clone());
    let _ = phone.sms.set(sms.clone());
    let _ = phone.screen.set(screen.clone());
    let _ = phone.share.set(share.clone());

    let port = std::env::var("FAKE_PHONE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(47120);
    let (name, kind) = if std::env::var_os("FAKE_PHONE_AS_PC").is_some() {
        ("Pairly Test PC", DeviceType::Desktop)
    } else {
        ("Pairly Test Phone", DeviceType::Phone)
    };
    let node = PairlyNode::builder(NodeConfig::new(name, kind))
        .keystore(Arc::new(FileKeyStore::new(dir.join("identity.key"))))
        .registry(Registry::open(&dir.join("registry.db"))?)
        .platform(phone.clone())
        .transport(LanTransport::new(LanConfig { port, mdns: true }))
        .plugin(Arc::new(PingPlugin))
        .plugin(notifications.clone())
        .plugin(clipboard.clone())
        .plugin(BatteryPlugin::new(phone.clone()))
        .plugin(findmy.clone())
        .plugin(share.clone())
        .plugin(media.clone())
        .plugin(telephony.clone())
        .plugin(ContactsPlugin::new(phone.clone()))
        .plugin(power.clone())
        .plugin(screen.clone())
        .plugin(FilesPlugin::new(phone.clone()))
        .plugin(sms.clone())
        .plugin(command.clone())
        .plugin(input.clone())
        .start()
        .await?;
    say(format!("started: id {}", node.device_id()));

    let mut events = node.subscribe();
    let n2 = node.clone();
    tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            match &e {
                NodeEvent::PairingRequested { id, name, code, .. } => {
                    say(format!("pairing with {name}, code {code}: accepting"));
                    let _ = n2.confirm_pair(*id, true);
                }
                NodeEvent::DeviceDiscovered { .. } | NodeEvent::DeviceLost { .. } => {}
                other => say(format!("event: {other:?}")),
            }
        }
    });

    // Commands from stdin.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let pc = || -> Option<DeviceId> {
        node.devices()
            .ok()?
            .into_iter()
            .find(|d| d.paired)
            .map(|d| d.id)
    };
    while let Some(line) = rx.recv().await {
        let line = line.trim().to_owned();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let result: anyhow::Result<()> = async {
            match cmd {
                "help" => say("devices | pair <id> | notify <id> <title>|<text> | unnotify <id> | call ringing|talking|missed|ended | sms <text> | laser show|move dx dy|hide | mouse dx dy | type <text> | cmds | run <name> | pcmedia <action> | ring on|off | watch start|stop | clip <text> | link <url> | power lock | ping | media | quit"),
                "devices" => {
                    for d in node.devices()? {
                        say(format!("{} {} paired={} link={:?}", d.id, d.name, d.paired, d.link));
                    }
                }
                "pair" => node.request_pair(rest.trim().parse().map_err(|_| anyhow::anyhow!("bad id"))?).await?,
                "notify" => {
                    let (id, text) = rest.split_once(' ').unwrap_or(("n1", rest));
                    let (title, body) = text.split_once('|').unwrap_or((text, ""));
                    notifications.posted(Notification {
                        id: id.into(),
                        app: "Chat".into(),
                        title: title.into(),
                        text: body.into(),
                        time: now_ms() as u64,
                        actions: vec![NotificationAction { key: "read".into(), label: "Mark as read".into() }],
                        can_reply: true,
                        icon: Some(Icon { width: 4, height: 4, rgba: (0..16).flat_map(|_| [0xE0, 0x60, 0x30, 0xFF]).collect() }),
                        silent: false,
                    });
                }
                "unnotify" => notifications.removed(rest.trim()),
                "call" => {
                    let state = match rest.trim() {
                        "ringing" => CallState::Ringing,
                        "talking" => CallState::Talking,
                        "missed" => CallState::Missed,
                        _ => CallState::Ended,
                    };
                    telephony.report(&CallEvent { state, number: Some("+919876543210".into()), contact: Some("Asha Verma".into()) })?;
                }
                "sms" => sms.new_message(
                    Message {
                        id: now_ms(),
                        thread_id: 1,
                        address: "+919876543210".into(),
                        body: rest.into(),
                        date_ms: now_ms(),
                        outgoing: false,
                        read: false,
                        participants: Vec::new(),
                        attachments: Vec::new(),
                    },
                    Some("Asha Verma".into()),
                ),
                "laser" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    let mut parts = rest.split_whitespace();
                    let action = match parts.next() {
                        Some("show") => LaserAction::Show,
                        Some("hide") => LaserAction::Hide,
                        _ => LaserAction::Move,
                    };
                    let dx = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let dy = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    input.laser(pc, LaserPointer { action, dx, dy })?;
                }
                "mouse" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    let mut parts = rest.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
                    let (dx, dy) = (parts.next().unwrap_or(0.0), parts.next().unwrap_or(0.0));
                    input.pointer(pc, PointerMotion { dx, dy, scroll_x: 0.0, scroll_y: 0.0 })?;
                }
                "type" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    input.key(pc, &KeyInput { text: Some(rest.into()), key: None, modifiers: Modifiers::default() })?;
                }
                "cmds" => say(format!("{:?}", phone.pc_commands.lock().expect("commands").iter().map(|c| (&c.id, &c.name)).collect::<Vec<_>>())),
                "run" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    let id = phone.pc_commands.lock().expect("commands").iter().find(|c| c.name == rest.trim() || c.id == rest.trim()).map(|c| c.id.clone()).ok_or_else(|| anyhow::anyhow!("no such command"))?;
                    command.run(pc, &id)?;
                }
                "pcmedia" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    let player = media.peer_players(pc).first().map(|p| p.id.clone()).ok_or_else(|| anyhow::anyhow!("the PC has no players"))?;
                    let action = match rest.trim() {
                        "play" => MediaAction::Play,
                        "pause" => MediaAction::Pause,
                        "next" => MediaAction::Next,
                        "previous" => MediaAction::Previous,
                        _ => MediaAction::PlayPause,
                    };
                    media.command(pc, &player, action)?;
                }
                "media" => media.local_changed(),
                "ring" => findmy.ring(pc().ok_or_else(|| anyhow::anyhow!("not paired"))?, rest.trim() == "on")?,
                "watch" => {
                    let pc = pc().ok_or_else(|| anyhow::anyhow!("not paired"))?;
                    let action = if rest.trim() == "stop" { ScreenAction::Stop } else { ScreenAction::Start };
                    screen.request(pc, action)?;
                }
                "clip" => clipboard.send_to(pc().ok_or_else(|| anyhow::anyhow!("not paired"))?, rest)?,
                "link" => share.send_text(pc().ok_or_else(|| anyhow::anyhow!("not paired"))?, rest, true)?,
                "power" => {
                    let action = match rest.trim() {
                        "restart" => PowerAction::Restart,
                        "off" => PowerAction::PowerOff,
                        _ => PowerAction::Lock,
                    };
                    say(format!("asking the PC to {action:?}"));
                    power.request(pc().ok_or_else(|| anyhow::anyhow!("not paired"))?, action).await?;
                }
                "ping" => {
                    node.send(pc().ok_or_else(|| anyhow::anyhow!("not paired"))?, pairly_plugins::ping::packet(Some("hello from the fake phone".into()))?)?;
                }
                "quit" => {
                    node.shutdown().await;
                    std::process::exit(0);
                }
                "" => {}
                other => say(format!("unknown command {other:?} (try help)")),
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => say(format!("ok: {line}")),
            Err(e) => say(format!("error: {line}: {e}")),
        }
    }
    Ok(())
}
