//! Media over MPRIS.
//!
//! - This PC's players (`org.mpris.MediaPlayer2.*` on the session bus) are watched and sent to
//!   paired devices, and their commands are carried out here.
//! - A paired device's player is published as an MPRIS player of its own
//!   (`org.mpris.MediaPlayer2.pairly_<id>`), so Waybar, `playerctl` and media keys control the
//!   phone like any local player.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use pairly_core::{DeviceId, PeerInfo};
use pairly_plugins::media::{MAX_ART_BYTES, MediaAction, MediaHost, MediaPlugin, PlayerState};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, fdo, interface};

use crate::dbus::DaemonIface;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// Changes come in bursts (a new track sets several properties); send one update.
const DEBOUNCE: Duration = Duration::from_millis(150);

pub enum Event {
    PeerPlayers(PeerInfo, Vec<PlayerState>),
    PeerArt(PeerInfo, String, Vec<u8>),
    Command(String, MediaAction),
}

struct Local {
    state: PlayerState,
    /// Needed for MPRIS `SetPosition`.
    track_id: Option<OwnedObjectPath>,
}

/// The plugin's view of this PC's media. Bus work happens in [`run`].
pub struct LinuxMedia {
    local: Mutex<Vec<Local>>,
    events: mpsc::UnboundedSender<Event>,
    art_dir: PathBuf,
    /// Artwork received from peers: (device, key) → cached file.
    peer_art: Mutex<HashMap<(DeviceId, String), PathBuf>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LinuxMedia {
    pub fn new(cache_dir: PathBuf) -> (Arc<Self>, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                local: Mutex::default(),
                events,
                art_dir: cache_dir.join("art"),
                peer_art: Mutex::default(),
            }),
            rx,
        )
    }

    /// A peer's players for the D-Bus API, with artwork as local URLs.
    pub fn to_dbus(&self, peer: DeviceId, players: &[PlayerState]) -> Vec<pairly_dbus::Player> {
        players
            .iter()
            .map(|p| pairly_dbus::Player {
                id: p.id.clone(),
                name: p.name.clone(),
                title: p.title.clone(),
                artist: p.artist.clone(),
                album: p.album.clone(),
                playing: p.playing,
                length_ms: p.length_ms.unwrap_or(0),
                position_ms: p.position_ms.unwrap_or(0),
                can_play: p.can_play,
                can_pause: p.can_pause,
                can_next: p.can_next,
                can_previous: p.can_previous,
                can_seek: p.can_seek,
                volume: p.volume.map_or(-1, i32::from),
                art_url: self.art_url(peer, p.art.as_deref()).unwrap_or_default(),
            })
            .collect()
    }

    fn art_url(&self, peer: DeviceId, key: Option<&str>) -> Option<String> {
        let key = key?;
        if key.starts_with("https://") {
            return Some(key.to_owned());
        }
        let path = lock(&self.peer_art).get(&(peer, key.to_owned()))?.clone();
        Some(format!("file://{}", path.display()))
    }
}

impl MediaHost for LinuxMedia {
    fn players(&self) -> Vec<PlayerState> {
        lock(&self.local).iter().map(|l| l.state.clone()).collect()
    }

    fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        let path = file_url_path(key)?;
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_ART_BYTES as u64 {
            return None;
        }
        std::fs::read(path).ok()
    }

    fn peer_players(&self, from: &PeerInfo, players: &[PlayerState]) {
        let _ = self
            .events
            .send(Event::PeerPlayers(from.clone(), players.to_vec()));
    }

    fn peer_artwork(&self, from: &PeerInfo, key: &str, data: &[u8]) {
        let _ = self
            .events
            .send(Event::PeerArt(from.clone(), key.to_owned(), data.to_vec()));
    }

    fn command(&self, _from: &PeerInfo, player: &str, action: MediaAction) {
        let _ = self.events.send(Event::Command(player.to_owned(), action));
    }
}

/// `file:///a%20b.png` → `/a b.png`.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(rest.get(i + 1..i + 3)?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

// ----- reading local players -------------------------------------------------------------

fn get<'a>(map: &'a HashMap<String, OwnedValue>, key: &str) -> Option<&'a Value<'a>> {
    map.get(key).map(|v| &**v)
}

fn as_str(v: Option<&Value<'_>>) -> String {
    match v {
        Some(Value::Str(s)) => s.to_string(),
        Some(Value::ObjectPath(p)) => p.to_string(),
        _ => String::new(),
    }
}

fn as_i64(v: Option<&Value<'_>>) -> Option<i64> {
    match v? {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        Value::F64(f) => Some(*f as i64),
        _ => None,
    }
}

fn as_bool(v: Option<&Value<'_>>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

fn artists(v: Option<&Value<'_>>) -> String {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}

fn us_to_ms(us: Option<i64>) -> Option<u64> {
    us.and_then(|u| u64::try_from(u).ok()).map(|u| u / 1000)
}

async fn read_player(conn: &Connection, name: &str) -> Option<Local> {
    let props = fdo::PropertiesProxy::builder(conn)
        .destination(name.to_owned())
        .ok()?
        .path(PATH)
        .ok()?
        .build()
        .await
        .ok()?;
    let player_iface = zbus::names::InterfaceName::try_from(PLAYER_IFACE).ok()?;
    let all = props.get_all(player_iface).await.ok()?;
    let root_iface = zbus::names::InterfaceName::try_from(ROOT_IFACE).ok()?;
    let identity = props
        .get(root_iface, "Identity")
        .await
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_else(|| name.trim_start_matches(PREFIX).to_owned());
    let metadata: HashMap<String, OwnedValue> = all
        .get("Metadata")
        .and_then(|v| HashMap::<String, OwnedValue>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    let art = as_str(get(&metadata, "mpris:artUrl"));
    let track_id = match get(&metadata, "mpris:trackid") {
        Some(Value::ObjectPath(p)) => Some(OwnedObjectPath::from(p.to_owned())),
        Some(Value::Str(s)) => ObjectPath::try_from(s.as_str())
            .ok()
            .map(|p| p.to_owned().into()),
        _ => None,
    };
    let status = as_str(get(&all, "PlaybackStatus"));
    let state = PlayerState {
        id: name.to_owned(),
        name: identity,
        title: as_str(get(&metadata, "xesam:title")),
        artist: artists(get(&metadata, "xesam:artist")),
        album: as_str(get(&metadata, "xesam:album")),
        art: (art.starts_with("file://") || art.starts_with("https://")).then_some(art),
        length_ms: us_to_ms(as_i64(get(&metadata, "mpris:length"))),
        position_ms: us_to_ms(as_i64(get(&all, "Position"))),
        playing: status == "Playing",
        can_play: as_bool(get(&all, "CanPlay")),
        can_pause: as_bool(get(&all, "CanPause")),
        can_next: as_bool(get(&all, "CanGoNext")),
        can_previous: as_bool(get(&all, "CanGoPrevious")),
        can_seek: as_bool(get(&all, "CanSeek")),
        volume: match get(&all, "Volume") {
            Some(Value::F64(v)) => Some((v.clamp(0.0, 1.0) * 100.0).round() as u8),
            _ => None,
        },
    };
    // Nothing loaded (a browser with no media tab): not worth showing.
    if state.title.is_empty() && status == "Stopped" {
        return None;
    }
    Some(Local { state, track_id })
}

async fn read_all(conn: &Connection) -> Vec<Local> {
    let names = match fdo::DBusProxy::new(conn).await {
        Ok(dbus) => dbus.list_names().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut out = Vec::new();
    for name in names {
        let name = name.as_str();
        if !name.starts_with(PREFIX) || is_ours_or_proxy(name) {
            continue;
        }
        if let Some(local) = read_player(conn, name).await {
            out.push(local);
        }
    }
    // Playing players first: receivers show the first one.
    out.sort_by_key(|l| !l.state.playing);
    out
}

/// playerctld re-exports other players, and `pairly_*` are our own mirrors of phones.
fn is_ours_or_proxy(name: &str) -> bool {
    let rest = name.trim_start_matches(PREFIX);
    rest.starts_with("playerctld") || rest.starts_with("pairly_")
}

/// Pause every playing local player (for a phone call). Returns the ones paused.
pub async fn pause_playing(conn: &Connection) -> Vec<String> {
    let mut paused = Vec::new();
    for local in read_all(conn).await {
        if local.state.playing
            && conn
                .call_method(
                    Some(local.state.id.as_str()),
                    PATH,
                    Some(PLAYER_IFACE),
                    "Pause",
                    &(),
                )
                .await
                .is_ok()
        {
            paused.push(local.state.id);
        }
    }
    if !paused.is_empty() {
        info!(players = ?paused, "paused media for a call");
    }
    paused
}

/// Resume players paused by [`pause_playing`].
pub async fn resume(conn: &Connection, players: &[String]) {
    for name in players {
        let _ = conn
            .call_method(Some(name.as_str()), PATH, Some(PLAYER_IFACE), "Play", &())
            .await;
    }
}

async fn run_command(conn: &Connection, media: &LinuxMedia, player: &str, action: MediaAction) {
    if !player.starts_with(PREFIX) || is_ours_or_proxy(player) {
        return;
    }
    let track = lock(&media.local)
        .iter()
        .find(|l| l.state.id == player)
        .and_then(|l| l.track_id.clone());
    let call = |method: &'static str| async move {
        conn.call_method(Some(player), PATH, Some(PLAYER_IFACE), method, &())
            .await
            .map(drop)
    };
    let result = match action {
        MediaAction::Play => call("Play").await,
        MediaAction::Pause => call("Pause").await,
        MediaAction::PlayPause => call("PlayPause").await,
        MediaAction::Stop => call("Stop").await,
        MediaAction::Next => call("Next").await,
        MediaAction::Previous => call("Previous").await,
        MediaAction::Seek(ms) => conn
            .call_method(
                Some(player),
                PATH,
                Some(PLAYER_IFACE),
                "Seek",
                &(ms.saturating_mul(1000),),
            )
            .await
            .map(drop),
        MediaAction::SetPosition(ms) => match track {
            Some(track) => conn
                .call_method(
                    Some(player),
                    PATH,
                    Some(PLAYER_IFACE),
                    "SetPosition",
                    &(
                        track,
                        i64::try_from(ms).unwrap_or(i64::MAX).saturating_mul(1000),
                    ),
                )
                .await
                .map(drop),
            None => Ok(()),
        },
        MediaAction::SetVolume(v) => conn
            .call_method(
                Some(player),
                PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(
                    PLAYER_IFACE,
                    "Volume",
                    Value::F64(f64::from(v.min(100)) / 100.0),
                ),
            )
            .await
            .map(drop),
    };
    match result {
        Ok(()) => debug!(player, ?action, "media command"),
        Err(e) => debug!(player, ?action, error = %e, "media command failed"),
    }
}

/// Watch this PC's players, carry out peers' commands and publish peers' players, until the
/// event channel closes.
pub async fn run(
    conn: Connection,
    media: Arc<LinuxMedia>,
    plugin: Arc<MediaPlugin>,
    mut events: mpsc::UnboundedReceiver<Event>,
) {
    let streams = async {
        let changed = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path(PATH)?
            .build();
        let seeked = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(PLAYER_IFACE)?
            .member("Seeked")?
            .build();
        let owners = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg0ns(PREFIX.trim_end_matches('.'))?
            .build();
        Ok::<_, zbus::Error>((
            MessageStream::for_match_rule(changed, &conn, None).await?,
            MessageStream::for_match_rule(seeked, &conn, None).await?,
            MessageStream::for_match_rule(owners, &conn, None).await?,
        ))
    };
    let (mut changed, mut seeked, mut owners) = match streams.await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "can't watch media players");
            return;
        }
    };
    let mut mirrors = Mirrors {
        conn: conn.clone(),
        media: media.clone(),
        plugin: plugin.clone(),
        players: HashMap::new(),
    };
    let mut refresh_at: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());
    loop {
        let sleep = async {
            match refresh_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = sleep => {
                refresh_at = None;
                let players = read_all(&conn).await;
                *lock(&media.local) = players;
                plugin.local_changed();
            }
            Some(_) = changed.next() => {
                refresh_at.get_or_insert_with(|| tokio::time::Instant::now() + DEBOUNCE);
            }
            Some(_) = seeked.next() => {
                refresh_at.get_or_insert_with(|| tokio::time::Instant::now() + DEBOUNCE);
            }
            Some(_) = owners.next() => {
                refresh_at.get_or_insert_with(|| tokio::time::Instant::now() + DEBOUNCE);
            }
            event = events.recv() => match event {
                Some(Event::Command(player, action)) => run_command(&conn, &media, &player, action).await,
                Some(Event::PeerPlayers(from, players)) => mirrors.update(from, players).await,
                Some(Event::PeerArt(from, key, data)) => mirrors.art(from, key, data).await,
                None => return,
            }
        }
    }
}

// ----- publishing peers' players ---------------------------------------------------------

struct Mirror {
    /// Its own connection: one MPRIS player per bus connection.
    conn: Connection,
}

struct Mirrors {
    conn: Connection,
    media: Arc<LinuxMedia>,
    plugin: Arc<MediaPlugin>,
    players: HashMap<DeviceId, Mirror>,
}

impl Mirrors {
    async fn notify_ui(&self, peer: DeviceId) {
        if let Ok(emitter) = SignalEmitter::new(&self.conn, pairly_dbus::OBJECT_PATH)
            && let Err(e) = DaemonIface::players_changed(&emitter, &peer.to_string()).await
        {
            debug!(error = %e, "can't emit PlayersChanged");
        }
    }

    async fn art(&mut self, from: PeerInfo, key: String, data: Vec<u8>) {
        let mut h = DefaultHasher::new();
        (from.id.to_string(), &key).hash(&mut h);
        let path = self.media.art_dir.join(format!("{:016x}", h.finish()));
        if std::fs::create_dir_all(&self.media.art_dir).is_err()
            || std::fs::write(&path, &data).is_err()
        {
            return;
        }
        lock(&self.media.peer_art).insert((from.id, key), path);
        // Refresh the mirror so its metadata points at the file.
        let players = self.plugin.peer_players(from.id);
        self.update(from, players).await;
    }

    async fn update(&mut self, from: PeerInfo, players: Vec<PlayerState>) {
        self.notify_ui(from.id).await;
        // Show the playing player, or the first one.
        let main = players
            .iter()
            .find(|p| p.playing)
            .or(players.first())
            .cloned();
        let Some(main) = main else {
            if self.players.remove(&from.id).is_some() {
                info!(device = %from.id, "phone player gone");
            }
            return;
        };
        let art_url = self.media.art_url(from.id, main.art.as_deref());
        let state = MirrorState {
            player: main,
            art_url,
            received: Instant::now(),
        };
        if let Some(mirror) = self.players.get(&from.id) {
            if let Err(e) = publish(&mirror.conn, state).await {
                debug!(error = %e, "can't update the mirrored player");
            }
            return;
        }
        let name = if from.name.is_empty() {
            from.id.to_string()
        } else {
            from.name.clone()
        };
        match create_mirror(from.id, name, state, self.plugin.clone()).await {
            Ok(conn) => {
                info!(device = %from.id, "publishing the phone's player over MPRIS");
                self.players.insert(from.id, Mirror { conn });
            }
            Err(e) => warn!(error = %e, "can't publish the phone's player"),
        }
    }
}

#[derive(Clone)]
struct MirrorState {
    player: PlayerState,
    art_url: Option<String>,
    received: Instant,
}

impl MirrorState {
    fn position_us(&self) -> i64 {
        let mut ms = self.player.position_ms.unwrap_or(0);
        if self.player.playing {
            ms += u64::try_from(self.received.elapsed().as_millis()).unwrap_or(0);
        }
        if let Some(len) = self.player.length_ms {
            ms = ms.min(len);
        }
        i64::try_from(ms).unwrap_or(i64::MAX).saturating_mul(1000)
    }
}

async fn create_mirror(
    peer: DeviceId,
    device_name: String,
    state: MirrorState,
    plugin: Arc<MediaPlugin>,
) -> zbus::Result<Connection> {
    let bus_name = format!("{PREFIX}pairly_{peer}");
    zbus::connection::Builder::session()?
        .name(bus_name)?
        .serve_at(PATH, MprisRoot { device_name })?
        .serve_at(
            PATH,
            MprisPlayer {
                peer,
                plugin,
                state,
            },
        )?
        .build()
        .await
}

async fn publish(conn: &Connection, state: MirrorState) -> zbus::Result<()> {
    let iface = conn
        .object_server()
        .interface::<_, MprisPlayer>(PATH)
        .await?;
    let emitter = iface.signal_emitter().clone();
    let mut player = iface.get_mut().await;
    player.state = state;
    player.playback_status_changed(&emitter).await?;
    player.metadata_changed(&emitter).await?;
    player.volume_changed(&emitter).await?;
    player.can_play_changed(&emitter).await?;
    player.can_pause_changed(&emitter).await?;
    player.can_go_next_changed(&emitter).await?;
    player.can_go_previous_changed(&emitter).await?;
    player.can_seek_changed(&emitter).await?;
    Ok(())
}

struct MprisRoot {
    device_name: String,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    async fn raise(&self) {}
    async fn quit(&self) {}
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> String {
        self.device_name.clone()
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "io.github.abhilesh1412.Pairly".into()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct MprisPlayer {
    peer: DeviceId,
    plugin: Arc<MediaPlugin>,
    state: MirrorState,
}

impl MprisPlayer {
    fn send(&self, action: MediaAction) {
        if let Err(e) = self
            .plugin
            .command(self.peer, &self.state.player.id, action)
        {
            debug!(error = %e, "can't send the media command");
        }
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    async fn play(&self) {
        self.send(MediaAction::Play);
    }
    async fn pause(&self) {
        self.send(MediaAction::Pause);
    }
    async fn play_pause(&self) {
        self.send(MediaAction::PlayPause);
    }
    async fn stop(&self) {
        self.send(MediaAction::Stop);
    }
    async fn next(&self) {
        self.send(MediaAction::Next);
    }
    async fn previous(&self) {
        self.send(MediaAction::Previous);
    }
    async fn seek(&self, offset: i64) {
        self.send(MediaAction::Seek(offset / 1000));
    }
    async fn set_position(&self, _track: ObjectPath<'_>, position: i64) {
        self.send(MediaAction::SetPosition(
            u64::try_from(position / 1000).unwrap_or(0),
        ));
    }
    async fn open_uri(&self, _uri: &str) {}

    #[zbus(property)]
    fn playback_status(&self) -> String {
        if self.state.player.playing {
            "Playing"
        } else {
            "Paused"
        }
        .into()
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let p = &self.state.player;
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        let mut put = |k: &str, v: Value<'_>| {
            if let Ok(v) = OwnedValue::try_from(v) {
                m.insert(k.to_owned(), v);
            }
        };
        put(
            "mpris:trackid",
            Value::from(ObjectPath::from_static_str_unchecked(
                "/io/github/abhilesh1412/Pairly/track",
            )),
        );
        put("xesam:title", Value::from(p.title.clone()));
        if !p.artist.is_empty() {
            put("xesam:artist", Value::from(vec![p.artist.clone()]));
        }
        if !p.album.is_empty() {
            put("xesam:album", Value::from(p.album.clone()));
        }
        if let Some(len) = p.length_ms {
            put(
                "mpris:length",
                Value::from(i64::try_from(len).unwrap_or(0).saturating_mul(1000)),
            );
        }
        if let Some(url) = &self.state.art_url {
            put("mpris:artUrl", Value::from(url.clone()));
        }
        m
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state
            .player
            .volume
            .map_or(1.0, |v| f64::from(v) / 100.0)
    }
    #[zbus(property)]
    fn set_volume(&mut self, volume: f64) {
        let percent = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.send(MediaAction::SetVolume(percent));
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.state.position_us()
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.state.player.can_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.state.player.can_previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.state.player.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.state.player.can_pause
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.state.player.can_seek
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_decode() {
        assert_eq!(
            file_url_path("file:///tmp/My%20Art.png"),
            Some(PathBuf::from("/tmp/My Art.png"))
        );
        assert_eq!(file_url_path("https://x/y.png"), None);
        assert!(is_ours_or_proxy("org.mpris.MediaPlayer2.playerctld"));
        assert!(is_ours_or_proxy("org.mpris.MediaPlayer2.pairly_abc"));
        assert!(!is_ours_or_proxy("org.mpris.MediaPlayer2.vlc"));
    }
}
