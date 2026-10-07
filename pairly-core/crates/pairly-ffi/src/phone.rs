//! Calls, text messages, PC commands and remote input for Kotlin.

use std::sync::Arc;

use pairly_core::PeerInfo;
use pairly_plugins::command::{CommandDone, CommandHost, CommandInfo};
use pairly_plugins::contacts::{Contact, ContactsHost};
use pairly_plugins::input::{
    ButtonAction, InputHost, KeyInput, Modifiers, MouseButton, PointerButton, PointerMotion,
    SpecialKey,
};
use pairly_plugins::sms::{Attachment, Conversation, Message, OutgoingAttachment, SmsHost};
use pairly_plugins::telephony::{CallAction, CallEvent, CallState, TelephonyHost};

use crate::PairlyError;

// ----- calls -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CallStateData {
    Ringing,
    Talking,
    Missed,
    Ended,
}

impl From<CallStateData> for CallState {
    fn from(s: CallStateData) -> Self {
        match s {
            CallStateData::Ringing => Self::Ringing,
            CallStateData::Talking => Self::Talking,
            CallStateData::Missed => Self::Missed,
            CallStateData::Ended => Self::Ended,
        }
    }
}

pub(crate) fn call_event(
    state: CallStateData,
    number: Option<String>,
    contact: Option<String>,
) -> CallEvent {
    CallEvent {
        state: state.into(),
        number,
        contact,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CallActionData {
    Answer,
    AnswerOnSpeaker,
    Reject,
    HangUp,
}

/// Implemented in Kotlin: act on the phone's calls for a paired PC.
#[uniffi::export(with_foreign)]
pub trait TelephonyHandler: Send + Sync {
    fn control(&self, from_name: String, action: CallActionData);
    fn dial(&self, from_name: String, number: String);
}

/// The phone reports its calls and takes orders about them; it doesn't show a PC's.
pub(crate) struct ForeignCalls(pub Arc<dyn TelephonyHandler>);

impl TelephonyHost for ForeignCalls {
    fn control(&self, from: &PeerInfo, action: CallAction) {
        let action = match action {
            CallAction::Answer => CallActionData::Answer,
            CallAction::AnswerOnSpeaker => CallActionData::AnswerOnSpeaker,
            CallAction::Reject => CallActionData::Reject,
            CallAction::HangUp => CallActionData::HangUp,
        };
        self.0.control(from.name.clone(), action);
    }

    fn dial(&self, from: &PeerInfo, number: &str) {
        self.0.dial(from.name.clone(), number.to_owned());
    }
}

// ----- files -------------------------------------------------------------------------------

/// Implemented in Kotlin: which folder paired PCs may browse. Called on Rust blocking threads.
#[uniffi::export(with_foreign)]
pub trait FilesHandler: Send + Sync {
    /// The shared storage root, or an error explaining the missing permission.
    fn root(&self) -> Result<String, PairlyError>;
}

pub(crate) struct ForeignFiles(pub Arc<dyn FilesHandler>);

impl pairly_plugins::files::FilesHost for ForeignFiles {
    fn root(&self) -> Result<std::path::PathBuf, String> {
        self.0.root().map(Into::into).map_err(|e| e.to_string())
    }
}

// ----- power -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PowerActionData {
    Lock,
    PowerOff,
    Restart,
}

/// Implemented in Kotlin: lock, power off or restart the phone because a PC asked. Called on a
/// Rust blocking thread; an error says why it couldn't (shown on the PC).
#[uniffi::export(with_foreign)]
pub trait PowerHandler: Send + Sync {
    fn act(&self, from: String, action: PowerActionData) -> Result<(), PairlyError>;
}

pub(crate) struct ForeignPower(pub Arc<dyn PowerHandler>);

impl pairly_plugins::power::PowerHost for ForeignPower {
    fn act(
        &self,
        from: &pairly_core::PeerInfo,
        action: pairly_plugins::power::PowerAction,
    ) -> Result<(), String> {
        use pairly_plugins::power::PowerAction;
        let action = match action {
            PowerAction::Lock => PowerActionData::Lock,
            PowerAction::PowerOff => PowerActionData::PowerOff,
            PowerAction::Restart => PowerActionData::Restart,
        };
        self.0
            .act(from.name.clone(), action)
            .map_err(|e| e.to_string())
    }
}

// ----- contacts ----------------------------------------------------------------------------

#[derive(Debug, Clone, uniffi::Record)]
pub struct ContactData {
    pub name: String,
    pub numbers: Vec<String>,
}

/// Implemented in Kotlin: the phone's contacts. Called on a Rust blocking thread.
#[uniffi::export(with_foreign)]
pub trait ContactsHandler: Send + Sync {
    fn contacts(&self) -> Result<Vec<ContactData>, PairlyError>;
}

pub(crate) struct ForeignContacts(pub Arc<dyn ContactsHandler>);

impl ContactsHost for ForeignContacts {
    fn contacts(&self) -> Result<Vec<Contact>, String> {
        self.0
            .contacts()
            .map(|list| {
                list.into_iter()
                    .map(|c| Contact {
                        name: c.name,
                        numbers: c.numbers,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
}

// ----- text messages -----------------------------------------------------------------------

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConversationData {
    pub thread_id: i64,
    pub addresses: Vec<String>,
    pub names: Vec<String>,
    pub snippet: String,
    pub date_ms: i64,
    pub read: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentData {
    pub part_id: i64,
    pub mime: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct OutgoingAttachmentData {
    pub mime: String,
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageData {
    pub id: i64,
    pub thread_id: i64,
    pub address: String,
    pub body: String,
    pub date_ms: i64,
    pub outgoing: bool,
    pub read: bool,
    /// Everyone in a group message (empty for plain texts).
    pub participants: Vec<String>,
    pub attachments: Vec<AttachmentData>,
}

impl From<MessageData> for Message {
    fn from(m: MessageData) -> Self {
        Self {
            id: m.id,
            thread_id: m.thread_id,
            address: m.address,
            body: m.body,
            date_ms: m.date_ms,
            outgoing: m.outgoing,
            read: m.read,
            participants: m.participants,
            attachments: m
                .attachments
                .into_iter()
                .map(|a| Attachment {
                    part_id: a.part_id,
                    mime: a.mime,
                    name: a.name,
                    size: a.size,
                })
                .collect(),
        }
    }
}

/// Implemented in Kotlin: reads the phone's SMS store and sends texts. Called on Rust
/// blocking threads.
#[uniffi::export(with_foreign)]
pub trait SmsHandler: Send + Sync {
    fn conversations(&self) -> Result<Vec<ConversationData>, PairlyError>;
    /// Oldest first, ending before `before_ms` (newest page when `None`).
    fn messages(
        &self,
        thread_id: i64,
        before_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<MessageData>, PairlyError>;
    /// Bytes of an MMS part, from `offset`, at most `len` (empty at the end).
    fn attachment(&self, part_id: i64, offset: u64, len: u32) -> Result<Vec<u8>, PairlyError>;
    /// A text, or an MMS when there are attachments or several addresses.
    fn send(
        &self,
        addresses: Vec<String>,
        text: String,
        attachments: Vec<OutgoingAttachmentData>,
    ) -> Result<(), PairlyError>;
}

pub(crate) struct ForeignSms(pub Arc<dyn SmsHandler>);

impl SmsHost for ForeignSms {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        self.0
            .conversations()
            .map(|list| {
                list.into_iter()
                    .map(|c| Conversation {
                        thread_id: c.thread_id,
                        addresses: c.addresses,
                        names: c.names,
                        snippet: c.snippet,
                        date_ms: c.date_ms,
                        read: c.read,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    fn messages(
        &self,
        thread_id: i64,
        before_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, String> {
        self.0
            .messages(thread_id, before_ms, limit)
            .map(|list| list.into_iter().map(Into::into).collect())
            .map_err(|e| e.to_string())
    }

    fn attachment(&self, part_id: i64, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        self.0
            .attachment(part_id, offset, len)
            .map_err(|e| e.to_string())
    }

    fn send(
        &self,
        addresses: &[String],
        text: &str,
        attachments: &[OutgoingAttachment],
    ) -> Result<(), String> {
        let attachments = attachments
            .iter()
            .map(|a| OutgoingAttachmentData {
                mime: a.mime.clone(),
                name: a.name.clone(),
                data: a.data.clone(),
            })
            .collect();
        self.0
            .send(addresses.to_vec(), text.to_owned(), attachments)
            .map_err(|e| e.to_string())
    }
}

// ----- commands ----------------------------------------------------------------------------

#[derive(Debug, Clone, uniffi::Record)]
pub struct CommandData {
    pub id: String,
    pub name: String,
}

/// Implemented in Kotlin: a PC's command list and results. Called on Rust threads.
#[uniffi::export(with_foreign)]
pub trait CommandHandler: Send + Sync {
    fn peer_commands(&self, from_id: String, from_name: String, commands: Vec<CommandData>);
    fn peer_finished(
        &self,
        from_id: String,
        from_name: String,
        id: String,
        success: bool,
        message: String,
    );
}

pub(crate) struct ForeignCommands(pub Arc<dyn CommandHandler>);

impl CommandHost for ForeignCommands {
    fn peer_commands(&self, from: &PeerInfo, commands: &[CommandInfo]) {
        self.0.peer_commands(
            from.id.to_string(),
            from.name.clone(),
            commands
                .iter()
                .map(|c| CommandData {
                    id: c.id.clone(),
                    name: c.name.clone(),
                })
                .collect(),
        );
    }

    fn peer_finished(&self, from: &PeerInfo, done: &CommandDone) {
        self.0.peer_finished(
            from.id.to_string(),
            from.name.clone(),
            done.id.clone(),
            done.success,
            done.message.clone(),
        );
    }
}

// ----- remote input ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MouseButtonData {
    Left,
    Right,
    Middle,
}

/// The presentation laser pointer: show it, move it, hide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LaserActionData {
    Show,
    Move,
    Hide,
}

pub(crate) fn laser(
    action: LaserActionData,
    dx: f32,
    dy: f32,
) -> pairly_plugins::input::LaserPointer {
    use pairly_plugins::input::LaserAction;
    pairly_plugins::input::LaserPointer {
        action: match action {
            LaserActionData::Show => LaserAction::Show,
            LaserActionData::Move => LaserAction::Move,
            LaserActionData::Hide => LaserAction::Hide,
        },
        dx,
        dy,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ButtonActionData {
    Click,
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum KeyData {
    Enter,
    Backspace,
    Delete,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Space,
    Function { number: u8 },
    VolumeUp,
    VolumeDown,
    Mute,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct ModifiersData {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

pub(crate) fn motion(dx: f32, dy: f32, scroll_x: f32, scroll_y: f32) -> PointerMotion {
    PointerMotion {
        dx,
        dy,
        scroll_x,
        scroll_y,
    }
}

pub(crate) fn button(button: MouseButtonData, action: ButtonActionData) -> PointerButton {
    PointerButton {
        button: match button {
            MouseButtonData::Left => MouseButton::Left,
            MouseButtonData::Right => MouseButton::Right,
            MouseButtonData::Middle => MouseButton::Middle,
        },
        action: match action {
            ButtonActionData::Click => ButtonAction::Click,
            ButtonActionData::Press => ButtonAction::Press,
            ButtonActionData::Release => ButtonAction::Release,
        },
    }
}

pub(crate) fn key(text: Option<String>, key: Option<KeyData>, m: ModifiersData) -> KeyInput {
    KeyInput {
        text,
        key: key.map(|k| match k {
            KeyData::Enter => SpecialKey::Enter,
            KeyData::Backspace => SpecialKey::Backspace,
            KeyData::Delete => SpecialKey::Delete,
            KeyData::Tab => SpecialKey::Tab,
            KeyData::Escape => SpecialKey::Escape,
            KeyData::Left => SpecialKey::Left,
            KeyData::Right => SpecialKey::Right,
            KeyData::Up => SpecialKey::Up,
            KeyData::Down => SpecialKey::Down,
            KeyData::Home => SpecialKey::Home,
            KeyData::End => SpecialKey::End,
            KeyData::PageUp => SpecialKey::PageUp,
            KeyData::PageDown => SpecialKey::PageDown,
            KeyData::Space => SpecialKey::Space,
            KeyData::Function { number } => SpecialKey::F(number),
            KeyData::VolumeUp => SpecialKey::VolumeUp,
            KeyData::VolumeDown => SpecialKey::VolumeDown,
            KeyData::Mute => SpecialKey::Mute,
        }),
        modifiers: Modifiers {
            ctrl: m.ctrl,
            alt: m.alt,
            shift: m.shift,
            logo: m.logo,
        },
    }
}

/// The phone sends input; it never applies a PC's.
pub(crate) struct NoInput;

impl InputHost for NoInput {
    fn pointer(&self, _: &PeerInfo, _: PointerMotion) {}
    fn button(&self, _: &PeerInfo, _: PointerButton) {}
    fn key(&self, _: &PeerInfo, _: &KeyInput) {}
}

// ----- screen sharing --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PointData {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ScreenKeyData {
    Back,
    Home,
    Recents,
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
}

/// Control of a shared screen; positions are fractions (0–1) of the screen.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ScreenInputData {
    Tap {
        x: f32,
        y: f32,
    },
    LongPress {
        x: f32,
        y: f32,
    },
    Swipe {
        points: Vec<PointData>,
        duration_ms: u32,
    },
    Key {
        key: ScreenKeyData,
    },
    Text {
        text: String,
    },
    Scroll {
        dx: f32,
        dy: f32,
    },
}

impl From<pairly_plugins::screen::ScreenInput> for ScreenInputData {
    fn from(input: pairly_plugins::screen::ScreenInput) -> Self {
        use pairly_plugins::screen::{ScreenInput, ScreenKey};
        match input {
            ScreenInput::Tap { x, y } => Self::Tap { x, y },
            ScreenInput::LongPress { x, y } => Self::LongPress { x, y },
            ScreenInput::Swipe {
                points,
                duration_ms,
            } => Self::Swipe {
                points: points
                    .into_iter()
                    .map(|(x, y)| PointData { x, y })
                    .collect(),
                duration_ms,
            },
            ScreenInput::Key { key } => Self::Key {
                key: match key {
                    ScreenKey::Back => ScreenKeyData::Back,
                    ScreenKey::Home => ScreenKeyData::Home,
                    ScreenKey::Recents => ScreenKeyData::Recents,
                    ScreenKey::Enter => ScreenKeyData::Enter,
                    ScreenKey::Backspace => ScreenKeyData::Backspace,
                    ScreenKey::Delete => ScreenKeyData::Delete,
                    ScreenKey::Left => ScreenKeyData::Left,
                    ScreenKey::Right => ScreenKeyData::Right,
                    ScreenKey::Up => ScreenKeyData::Up,
                    ScreenKey::Down => ScreenKeyData::Down,
                },
            },
            ScreenInput::Text { text } => Self::Text { text },
            ScreenInput::Scroll { dx, dy } => Self::Scroll { dx, dy },
        }
    }
}

/// Implemented in Kotlin: share this phone's screen with a PC that asks (after the user
/// agrees), stop, and act on the PC's taps and keys.
#[uniffi::export(with_foreign)]
pub trait ScreenHandler: Send + Sync {
    /// A PC asks to see the screen. Ask the user; once recording, call `Node::screen_started`.
    fn start_sharing(&self, device_id: String, device_name: String) -> Result<(), PairlyError>;
    fn stop_sharing(&self, device_id: String);
    fn input(&self, device_id: String, input: ScreenInputData);
    /// The PC went away mid-stream.
    fn disconnected(&self, device_id: String);

    // Watching a PC's screen (after `Node::screen_request`).
    /// The PC's stream starts (`width` × `height`).
    fn viewer_started(&self, device_id: String, width: u32, height: u32);
    /// One H.264 access unit from the PC (`key`: decoding can start here).
    fn viewer_frame(&self, device_id: String, data: Vec<u8>, key: bool);
    /// The PC's stream ended (or was refused), and why.
    fn viewer_stopped(&self, device_id: String, reason: String);
}

pub(crate) struct ForeignScreen(pub Arc<dyn ScreenHandler>);

impl pairly_plugins::screen::ScreenHost for ForeignScreen {
    fn start_sharing(&self, from: &pairly_core::PeerInfo) -> Result<(), String> {
        self.0
            .start_sharing(from.id.to_string(), from.name.clone())
            .map_err(|e| e.to_string())
    }

    fn stop_sharing(&self, from: &pairly_core::PeerInfo) {
        self.0.stop_sharing(from.id.to_string());
    }

    fn input(&self, from: &pairly_core::PeerInfo, input: pairly_plugins::screen::ScreenInput) {
        self.0.input(from.id.to_string(), input.into());
    }

    fn disconnected(&self, peer: pairly_core::DeviceId) {
        self.0.disconnected(peer.to_string());
        self.0
            .viewer_stopped(peer.to_string(), "the PC disconnected".into());
    }

    fn started(&self, from: &pairly_core::PeerInfo, width: u32, height: u32) {
        self.0.viewer_started(from.id.to_string(), width, height);
    }

    fn frame(&self, from: &pairly_core::PeerInfo, frame: pairly_plugins::screen::ScreenFrame) {
        self.0
            .viewer_frame(from.id.to_string(), frame.data, frame.key || frame.config);
    }

    fn stopped(&self, from: &pairly_core::PeerInfo, reason: &str) {
        self.0
            .viewer_stopped(from.id.to_string(), reason.to_owned());
    }
}

impl From<ScreenInputData> for pairly_plugins::screen::ScreenInput {
    fn from(input: ScreenInputData) -> Self {
        use pairly_plugins::screen::{ScreenInput, ScreenKey};
        match input {
            ScreenInputData::Tap { x, y } => ScreenInput::Tap { x, y },
            ScreenInputData::LongPress { x, y } => ScreenInput::LongPress { x, y },
            ScreenInputData::Swipe {
                points,
                duration_ms,
            } => ScreenInput::Swipe {
                points: points.into_iter().map(|p| (p.x, p.y)).collect(),
                duration_ms,
            },
            ScreenInputData::Key { key } => ScreenInput::Key {
                key: match key {
                    ScreenKeyData::Back => ScreenKey::Back,
                    ScreenKeyData::Home => ScreenKey::Home,
                    ScreenKeyData::Recents => ScreenKey::Recents,
                    ScreenKeyData::Enter => ScreenKey::Enter,
                    ScreenKeyData::Backspace => ScreenKey::Backspace,
                    ScreenKeyData::Delete => ScreenKey::Delete,
                    ScreenKeyData::Left => ScreenKey::Left,
                    ScreenKeyData::Right => ScreenKey::Right,
                    ScreenKeyData::Up => ScreenKey::Up,
                    ScreenKeyData::Down => ScreenKey::Down,
                },
            },
            ScreenInputData::Text { text } => ScreenInput::Text { text },
            ScreenInputData::Scroll { dx, dy } => ScreenInput::Scroll { dx, dy },
        }
    }
}
