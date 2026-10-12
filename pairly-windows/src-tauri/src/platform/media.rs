//! This PC's media players, for paired phones: what's playing in each app that uses Windows'
//! media controls (Spotify, browsers, Media Player…), with artwork, and the phone's play,
//! pause, skip and seek carried out here. Read once a second on a thread of its own, which
//! also runs the commands (Windows' media sessions are slow to call, so nothing waits on it).

use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pairly_plugins::media::{MediaAction, PlayerState};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Cmd {
    Control(String, MediaAction),
    /// Pause whatever plays; answer with the players paused.
    PausePlaying(Sender<Vec<String>>),
    Resume(Vec<String>),
}

pub struct LocalMedia {
    players: Mutex<Vec<PlayerState>>,
    /// Artwork of the players above: key → image bytes.
    art: Mutex<HashMap<String, Vec<u8>>>,
    tx: Mutex<Option<Sender<Cmd>>>,
}

impl LocalMedia {
    /// Start watching; `changed` runs whenever a player starts, stops or changes track or state.
    pub fn start(changed: impl Fn() + Send + 'static) -> Arc<Self> {
        let this = Arc::new(Self {
            players: Mutex::default(),
            art: Mutex::default(),
            tx: Mutex::new(None),
        });
        let (tx, rx) = channel();
        *lock(&this.tx) = Some(tx);
        let weak = Arc::downgrade(&this);
        let spawned = std::thread::Builder::new()
            .name("pairly-media".into())
            .spawn(move || imp::run(&weak, &rx, &changed));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no media thread");
        }
        this
    }

    pub fn players(&self) -> Vec<PlayerState> {
        lock(&self.players).clone()
    }

    pub fn artwork(&self, key: &str) -> Option<Vec<u8>> {
        lock(&self.art).get(key).cloned()
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(cmd);
        }
    }

    pub fn control(&self, player: &str, action: MediaAction) {
        self.send(Cmd::Control(player.to_owned(), action));
    }

    /// Pause every playing player (a call is ringing); returns which, to resume later.
    pub fn pause_playing(&self) -> Vec<String> {
        let (tx, rx) = channel();
        self.send(Cmd::PausePlaying(tx));
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or_default()
    }

    pub fn resume(&self, players: Vec<String>) {
        if !players.is_empty() {
            self.send(Cmd::Resume(players));
        }
    }

    /// Store a new picture of the players (from the media thread); true if anything changed
    /// that a phone shows (not just the position moving on).
    fn update(&self, players: Vec<PlayerState>, art: HashMap<String, Vec<u8>>) -> bool {
        let mut current = lock(&self.players);
        let strip = |p: &PlayerState| PlayerState {
            position_ms: None,
            ..p.clone()
        };
        let same = current.len() == players.len()
            && current.iter().zip(&players).all(|(a, b)| {
                strip(a) == strip(b) && {
                    // A jump (seek) counts as a change; playing on doesn't.
                    let (pa, pb) = (a.position_ms.unwrap_or(0), b.position_ms.unwrap_or(0));
                    pa.abs_diff(pb) < 3000 || b.playing
                }
            });
        *current = players;
        *lock(&self.art) = art;
        !same
    }
}

/// A short name for the app behind a media session ("Spotify.exe" → "Spotify").
pub fn app_name(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let known = [
        ("msedge", "Microsoft Edge"),
        ("chrome", "Google Chrome"),
        ("firefox", "Firefox"),
        ("spotify", "Spotify"),
        ("zunemusic", "Media Player"),
        ("zunevideo", "Movies & TV"),
        ("vlc", "VLC"),
        ("brave", "Brave"),
        ("opera", "Opera"),
        ("applemusic", "Apple Music"),
        ("itunes", "iTunes"),
        ("foobar2000", "foobar2000"),
        ("musicbee", "MusicBee"),
    ];
    if let Some((_, name)) = known.iter().find(|(k, _)| lower.contains(k)) {
        return (*name).to_owned();
    }
    // "Publisher.App_hash!App" → "App"; "C:\…\app.exe" → "app".
    let base = id.rsplit(['!', '\\', '/']).next().unwrap_or(id);
    let base = base
        .strip_suffix(".exe")
        .or(base.strip_suffix(".EXE"))
        .unwrap_or(base);
    let base = base.split('_').next().unwrap_or(base);
    let base = base.rsplit('.').next().unwrap_or(base);
    if base.is_empty() {
        "Media".to_owned()
    } else {
        base.to_owned()
    }
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};
    use std::sync::Weak;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use pairly_plugins::media::{MAX_ART_BYTES, MediaAction, PlayerState};
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Storage::Streams::DataReader;

    use super::{Cmd, LocalMedia, app_name};

    /// 100-nanosecond ticks in a millisecond.
    const TICKS_PER_MS: i64 = 10_000;

    /// Artwork already read: (player, title) → (key, bytes), so it's read once per track.
    type ArtCache = HashMap<(String, String), Option<(String, Vec<u8>)>>;

    pub fn run(this: &Weak<LocalMedia>, rx: &Receiver<Cmd>, changed: &dyn Fn()) {
        let manager = match Manager::RequestAsync().and_then(|op| op.join()) {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(error = %e, "no media sessions on this PC");
                return;
            }
        };
        let mut art_cache = ArtCache::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(1000)) {
                Ok(cmd) => handle(&manager, cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(this) = this.upgrade() else { return };
            let (players, art) = read(&manager, &mut art_cache);
            if this.update(players, art) {
                changed();
            }
        }
    }

    fn sessions(manager: &Manager) -> Vec<Session> {
        manager
            .GetSessions()
            .map(|list| list.into_iter().collect())
            .unwrap_or_default()
    }

    fn id_of(s: &Session) -> String {
        s.SourceAppUserModelId()
            .map(|h| h.to_string())
            .unwrap_or_default()
    }

    fn read(
        manager: &Manager,
        cache: &mut ArtCache,
    ) -> (Vec<PlayerState>, HashMap<String, Vec<u8>>) {
        let mut players = Vec::new();
        let mut art = HashMap::new();
        let mut seen = Vec::new();
        for s in sessions(manager) {
            let id = id_of(&s);
            if id.is_empty() || seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            let Ok(props) = s.TryGetMediaPropertiesAsync().and_then(|op| op.join()) else {
                continue;
            };
            let text = |h: windows::core::Result<windows::core::HSTRING>| {
                h.map(|h| h.to_string()).unwrap_or_default()
            };
            let title = text(props.Title());
            let mut artist = text(props.Artist());
            if artist.is_empty() {
                artist = text(props.AlbumArtist());
            }
            let album = text(props.AlbumTitle());
            let info = s.GetPlaybackInfo().ok();
            let controls = info.as_ref().and_then(|i| i.Controls().ok());
            let can = |f: &dyn Fn(
                &windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackControls,
            ) -> windows::core::Result<bool>| {
                controls.as_ref().and_then(|c| f(c).ok()).unwrap_or(false)
            };
            let playing = info
                .as_ref()
                .and_then(|i| i.PlaybackStatus().ok())
                .is_some_and(|st| st == Status::Playing);
            let timeline = s.GetTimelineProperties().ok();
            let (length_ms, position_ms) = match &timeline {
                Some(t) => {
                    let end = t.EndTime().map(|d| d.Duration).unwrap_or(0)
                        - t.StartTime().map(|d| d.Duration).unwrap_or(0);
                    let mut pos = t.Position().map(|d| d.Duration).unwrap_or(0);
                    // The position is as of the last update: move it on while playing.
                    if playing
                        && let Ok(updated) = t.LastUpdatedTime()
                        && let Some(age) = ticks_since(updated.UniversalTime)
                    {
                        pos += age;
                    }
                    let to_ms = |t: i64| u64::try_from(t / TICKS_PER_MS).ok();
                    let length = to_ms(end).filter(|&l| l > 0);
                    let position = to_ms(pos).map(|p| length.map_or(p, |l| p.min(l)));
                    (length, position)
                }
                None => (None, None),
            };
            let key = (id.clone(), format!("{title}\u{1}{artist}\u{1}{album}"));
            let art_entry = cache
                .entry(key)
                .or_insert_with(|| thumbnail(&props).map(|bytes| (art_key(&bytes), bytes)));
            if let Some((k, bytes)) = art_entry {
                art.insert(k.clone(), bytes.clone());
            }
            players.push(PlayerState {
                name: app_name(&id),
                id,
                title,
                artist,
                album,
                art: art_entry.as_ref().map(|(k, _)| k.clone()),
                length_ms,
                position_ms,
                playing,
                can_play: can(&|c| c.IsPlayEnabled()),
                can_pause: can(&|c| c.IsPauseEnabled()),
                can_next: can(&|c| c.IsNextEnabled()),
                can_previous: can(&|c| c.IsPreviousEnabled()),
                can_seek: can(&|c| c.IsPlaybackPositionEnabled()) && length_ms.is_some(),
                volume: None,
            });
        }
        // Forget artwork of sessions that are gone.
        cache.retain(|(id, _), _| seen.contains(id));
        if cache.len() > 50 {
            cache.clear();
        }
        // The playing ones first.
        players.sort_by_key(|p| !p.playing);
        (players, art)
    }

    /// 100 ns ticks from a Windows time (ticks since 1601) until now.
    fn ticks_since(universal: i64) -> Option<i64> {
        const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let now = i64::try_from(now.as_nanos() / 100).ok()? + EPOCH_DIFF;
        let age = now - universal;
        (0..i64::from(u32::MAX)).contains(&age).then_some(age)
    }

    fn art_key(bytes: &[u8]) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        format!("win-art-{:016x}", h.finish())
    }

    fn thumbnail(
        props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
    ) -> Option<Vec<u8>> {
        let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
        let size = u32::try_from(stream.Size().ok()?).ok()?;
        if size == 0 || size as usize > MAX_ART_BYTES {
            return None;
        }
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
        let got = reader.LoadAsync(size).ok()?.join().ok()?;
        let mut bytes = vec![0u8; got as usize];
        reader.ReadBytes(&mut bytes).ok()?;
        Some(bytes)
    }

    fn handle(manager: &Manager, cmd: Cmd) {
        match cmd {
            Cmd::Control(player, action) => {
                if let Some(s) = sessions(manager).into_iter().find(|s| id_of(s) == player)
                    && let Err(e) = control(&s, action)
                {
                    tracing::debug!(error = %e, player, "media command failed");
                }
            }
            Cmd::PausePlaying(answer) => {
                let mut paused = Vec::new();
                for s in sessions(manager) {
                    let playing = s
                        .GetPlaybackInfo()
                        .and_then(|i| i.PlaybackStatus())
                        .is_ok_and(|st| st == Status::Playing);
                    if playing && s.TryPauseAsync().and_then(|op| op.join()).unwrap_or(false) {
                        paused.push(id_of(&s));
                    }
                }
                let _ = answer.send(paused);
            }
            Cmd::Resume(players) => {
                for s in sessions(manager) {
                    if players.contains(&id_of(&s)) {
                        let _ = s.TryPlayAsync().and_then(|op| op.join());
                    }
                }
            }
        }
    }

    fn control(s: &Session, action: MediaAction) -> windows::core::Result<bool> {
        match action {
            MediaAction::Play => s.TryPlayAsync()?.join(),
            MediaAction::Pause => s.TryPauseAsync()?.join(),
            MediaAction::PlayPause => s.TryTogglePlayPauseAsync()?.join(),
            MediaAction::Stop => s.TryStopAsync()?.join(),
            MediaAction::Next => s.TrySkipNextAsync()?.join(),
            MediaAction::Previous => s.TrySkipPreviousAsync()?.join(),
            MediaAction::Seek(delta_ms) => {
                let t = s.GetTimelineProperties()?;
                let now = t.Position()?.Duration;
                let target = (now + delta_ms * TICKS_PER_MS).max(0);
                s.TryChangePlaybackPositionAsync(target)?.join()
            }
            MediaAction::SetPosition(ms) => {
                let ticks = i64::try_from(ms).unwrap_or(i64::MAX / TICKS_PER_MS) * TICKS_PER_MS;
                s.TryChangePlaybackPositionAsync(ticks)?.join()
            }
            // Windows' media sessions have no volume of their own.
            MediaAction::SetVolume(_) => Ok(false),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::Weak;
    use std::sync::mpsc::Receiver;

    use super::{Cmd, LocalMedia};

    /// Other systems: no players (the app builds there only for working on the window).
    pub fn run(_this: &Weak<LocalMedia>, rx: &Receiver<Cmd>, _changed: &dyn Fn()) {
        for cmd in rx {
            if let Cmd::PausePlaying(answer) = cmd {
                let _ = answer.send(Vec::new());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name("MSEdge"), "Microsoft Edge");
        assert_eq!(
            app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Media Player"
        );
        assert_eq!(app_name("C:\\Apps\\cool player.exe"), "cool player");
        assert_eq!(app_name("Contoso.Tunes_abc123!App"), "App");
    }
}
