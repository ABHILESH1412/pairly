//! Calls, text messages, commands and remote input between a "phone" and a "PC" node over the
//! in-memory transport.
#![allow(clippy::unwrap_used)] // test helpers

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pairly_core::memory::MemoryNetwork;
use pairly_core::{DeviceType, NodeConfig, NodeEvent, PairlyNode, PeerInfo};
use pairly_plugins::command::{CommandDone, CommandHost, CommandInfo, CommandPlugin};
use pairly_plugins::contacts::{Contact, ContactsHost, ContactsPlugin};
use pairly_plugins::input::{
    ButtonAction, InputHost, InputPlugin, KeyInput, LaserAction, LaserPointer, Modifiers,
    MouseButton, PointerButton, PointerMotion, SpecialKey,
};
use pairly_plugins::power::{PowerAction, PowerHost, PowerPlugin};
use pairly_plugins::sms::{Conversation, Message, SmsHost, SmsPlugin};
use pairly_plugins::telephony::{CallAction, CallEvent, CallState, TelephonyHost, TelephonyPlugin};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Call {
    Telephony(CallState, String),
    Control(CallAction),
    Dial(String),
    Power(PowerAction),
    SmsSend(Vec<String>, String),
    SmsNew(String),
    Commands(Vec<String>),
    Run(String),
    Done(bool, String),
    Pointer(f32, f32),
    Button(MouseButton),
    Key(Option<String>, Option<SpecialKey>, bool),
    Laser(LaserAction, f32, f32),
}

struct Host {
    phone: bool,
    calls: Mutex<mpsc::UnboundedSender<Call>>,
}

impl Host {
    fn call(&self, c: Call) {
        let _ = self.calls.lock().unwrap().send(c);
    }
}

fn message(id: i64, body: &str) -> Message {
    Message {
        id,
        thread_id: 7,
        address: "+15551234".into(),
        body: body.into(),
        date_ms: 1000 * id,
        outgoing: false,
        read: true,
        participants: vec![],
        attachments: vec![],
    }
}

impl TelephonyHost for Host {
    fn call(&self, _: &PeerInfo, e: &CallEvent) {
        self.call(Call::Telephony(e.state, e.caller().to_owned()));
    }
    fn control(&self, _: &PeerInfo, action: CallAction) {
        self.call(Call::Control(action));
    }
    fn dial(&self, _: &PeerInfo, number: &str) {
        self.call(Call::Dial(number.to_owned()));
    }
}

impl PowerHost for Host {
    fn act(&self, _: &PeerInfo, action: PowerAction) -> Result<(), String> {
        if !self.phone {
            return Err("not a phone".into());
        }
        if action == PowerAction::PowerOff {
            return Err("needs the accessibility permission".into());
        }
        self.call(Call::Power(action));
        Ok(())
    }
}

impl ContactsHost for Host {
    fn contacts(&self) -> Result<Vec<Contact>, String> {
        if !self.phone {
            return Err("not a phone".into());
        }
        Ok(vec![
            Contact {
                name: "bob".into(),
                numbers: vec!["+2".into()],
            },
            Contact {
                name: "Alice".into(),
                numbers: vec!["+1".into(), "+3".into()],
            },
        ])
    }
}

impl SmsHost for Host {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        if !self.phone {
            return Err("not a phone".into());
        }
        Ok(vec![Conversation {
            thread_id: 7,
            addresses: vec!["+15551234".into()],
            names: vec!["Alice".into()],
            snippet: "see you".into(),
            date_ms: 3000,
            read: false,
        }])
    }
    fn messages(
        &self,
        thread: i64,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, String> {
        assert_eq!((thread, before, limit), (7, Some(3000), 50));
        Ok(vec![message(1, "hi"), message(2, "see you")])
    }
    fn attachment(&self, part: i64, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        assert_eq!(part, 42);
        let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
        let start = usize::try_from(offset).unwrap().min(data.len());
        let end = (start + len as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }
    fn send(
        &self,
        addresses: &[String],
        text: &str,
        _: &[pairly_plugins::sms::OutgoingAttachment],
    ) -> Result<(), String> {
        self.call(Call::SmsSend(addresses.to_vec(), text.to_owned()));
        Ok(())
    }
    fn received(&self, _: &PeerInfo, m: &Message, _: Option<&str>) {
        self.call(Call::SmsNew(m.body.clone()));
    }
}

impl CommandHost for Host {
    fn commands(&self) -> Vec<CommandInfo> {
        if self.phone {
            return Vec::new();
        }
        vec![CommandInfo {
            id: "lock".into(),
            name: "Lock screen".into(),
        }]
    }
    fn run(&self, _: &PeerInfo, id: &str) {
        self.call(Call::Run(id.into()));
    }
    fn peer_commands(&self, _: &PeerInfo, commands: &[CommandInfo]) {
        self.call(Call::Commands(
            commands.iter().map(|c| c.name.clone()).collect(),
        ));
    }
    fn peer_finished(&self, _: &PeerInfo, done: &CommandDone) {
        self.call(Call::Done(done.success, done.message.clone()));
    }
}

impl InputHost for Host {
    fn pointer(&self, _: &PeerInfo, m: PointerMotion) {
        self.call(Call::Pointer(m.dx, m.dy));
    }
    fn button(&self, _: &PeerInfo, b: PointerButton) {
        self.call(Call::Button(b.button));
    }
    fn key(&self, _: &PeerInfo, k: &KeyInput) {
        self.call(Call::Key(k.text.clone(), k.key, k.modifiers.ctrl));
    }
    fn laser(&self, _: &PeerInfo, l: LaserPointer) {
        self.call(Call::Laser(l.action, l.dx, l.dy));
    }
}

struct Side {
    node: PairlyNode,
    telephony: Arc<TelephonyPlugin>,
    sms: Arc<SmsPlugin>,
    command: Arc<CommandPlugin>,
    input: Arc<InputPlugin>,
    contacts: Arc<ContactsPlugin>,
    power: Arc<PowerPlugin>,
    calls: mpsc::UnboundedReceiver<Call>,
}

impl Side {
    async fn next(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(5), self.calls.recv())
            .await
            .expect("call in time")
            .unwrap()
    }
}

async fn start(net: &MemoryNetwork, addr: &str, phone: bool) -> Side {
    let (tx, calls) = mpsc::unbounded_channel();
    let host = Arc::new(Host {
        phone,
        calls: Mutex::new(tx),
    });
    let telephony = TelephonyPlugin::new(host.clone());
    let sms = SmsPlugin::new(host.clone());
    let command = CommandPlugin::new(host.clone());
    let input = InputPlugin::new(host.clone());
    let power = PowerPlugin::new(host.clone());
    let contacts = ContactsPlugin::new(host);
    let node = PairlyNode::builder(NodeConfig::new(addr, DeviceType::Phone))
        .transport(net.transport(addr))
        .plugin(telephony.clone())
        .plugin(sms.clone())
        .plugin(command.clone())
        .plugin(input.clone())
        .plugin(contacts.clone())
        .plugin(power.clone())
        .start()
        .await
        .unwrap();
    Side {
        node,
        telephony,
        sms,
        command,
        input,
        contacts,
        power,
        calls,
    }
}

async fn pair(a: &Side, b: &Side) {
    let (mut ae, mut be) = (a.node.subscribe(), b.node.subscribe());
    let (a_id, b_id) = (a.node.device_id(), b.node.device_id());
    while !a.node.devices().unwrap().iter().any(|d| d.id == b_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.node.request_pair(b_id).await.unwrap();
    for (rx, node, peer) in [(&mut ae, &a.node, b_id), (&mut be, &b.node, a_id)] {
        loop {
            if let NodeEvent::PairingRequested { .. } = rx.recv().await.unwrap() {
                node.confirm_pair(peer, true).unwrap();
                break;
            }
        }
    }
    for rx in [&mut ae, &mut be] {
        while !matches!(rx.recv().await.unwrap(), NodeEvent::Connected { .. }) {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_texts_commands_and_input() {
    let net = MemoryNetwork::new();
    let mut phone = start(&net, "phone", true).await;
    let mut pc = start(&net, "pc", false).await;
    pair(&phone, &pc).await;
    let (phone_id, pc_id) = (phone.node.device_id(), pc.node.device_id());

    // On connect the PC publishes its commands; the phone (empty list) publishes none.
    assert_eq!(
        phone.next().await,
        Call::Commands(vec!["Lock screen".into()])
    );
    assert_eq!(pc.next().await, Call::Commands(vec![]));

    // Calls.
    let ring = CallEvent {
        state: CallState::Ringing,
        number: Some("+15551234".into()),
        contact: Some("Alice".into()),
    };
    phone.telephony.report(&ring).unwrap();
    assert_eq!(
        pc.next().await,
        Call::Telephony(CallState::Ringing, "Alice".into())
    );

    pc.telephony
        .control(phone_id, CallAction::AnswerOnSpeaker)
        .unwrap();
    assert_eq!(
        phone.next().await,
        Call::Control(CallAction::AnswerOnSpeaker)
    );
    pc.telephony.dial(phone_id, " +15551234 ").unwrap();
    assert_eq!(phone.next().await, Call::Dial("+15551234".into()));

    // Contacts, sorted by name.
    let contacts = pc.contacts.fetch(phone_id).await.unwrap();
    assert_eq!(
        contacts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["Alice", "bob"]
    );
    assert!(phone.contacts.fetch(pc_id).await.is_err());

    // Power: lock works; a refusal comes back as an error.
    pc.power.request(phone_id, PowerAction::Lock).await.unwrap();
    assert_eq!(phone.next().await, Call::Power(PowerAction::Lock));
    let err = pc.power.request(phone_id, PowerAction::PowerOff).await;
    assert!(err.unwrap_err().to_string().contains("accessibility"));
    assert!(phone.power.request(pc_id, PowerAction::Lock).await.is_err());

    // Texts: the PC asks, the phone answers.
    let convs = pc.sms.conversations(phone_id).await.unwrap();
    assert_eq!((convs.len(), convs[0].names[0].as_str()), (1, "Alice"));
    let msgs = pc.sms.messages(phone_id, 7, Some(3000), 50).await.unwrap();
    assert_eq!(
        msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        ["hi", "see you"]
    );
    // The phone asking a PC gets an error, not a hang.
    assert!(phone.sms.conversations(pc_id).await.is_err());
    let picture = pc.sms.attachment(phone_id, 42, 10_000_000).await.unwrap();
    assert_eq!(picture.len(), 2_000_000, "fetched in several chunks");
    assert!(
        pc.sms.attachment(phone_id, 42, 1000).await.is_err(),
        "size cap"
    );
    pc.sms
        .send(phone_id, vec!["+15551234".into()], "on my way", vec![])
        .unwrap();
    assert_eq!(
        phone.next().await,
        Call::SmsSend(vec!["+15551234".into()], "on my way".into())
    );
    phone
        .sms
        .new_message(message(3, "great"), Some("Alice".into()));
    assert_eq!(pc.next().await, Call::SmsNew("great".into()));

    // Commands: only published ids run.
    phone.command.run(pc_id, "lock").unwrap();
    assert_eq!(pc.next().await, Call::Run("lock".into()));
    pc.command.finished(phone_id, "lock", true, "done");
    assert_eq!(phone.next().await, Call::Done(true, "done".into()));
    phone.command.run(pc_id, "rm -rf /").unwrap();
    assert_eq!(
        phone.next().await,
        Call::Done(false, "no such command on this PC".into())
    );

    // Input.
    phone
        .input
        .pointer(
            pc_id,
            PointerMotion {
                dx: 3.0,
                dy: -2.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
            },
        )
        .unwrap();
    assert_eq!(pc.next().await, Call::Pointer(3.0, -2.0));
    phone
        .input
        .button(
            pc_id,
            PointerButton {
                button: MouseButton::Right,
                action: ButtonAction::Click,
            },
        )
        .unwrap();
    assert_eq!(pc.next().await, Call::Button(MouseButton::Right));
    let key = KeyInput {
        text: None,
        key: Some(SpecialKey::Tab),
        modifiers: Modifiers {
            ctrl: true,
            ..Default::default()
        },
    };
    phone.input.key(pc_id, &key).unwrap();
    assert_eq!(
        pc.next().await,
        Call::Key(None, Some(SpecialKey::Tab), true)
    );

    // Laser pointer: shown, moved (by a fraction of the screen, clamped), hidden.
    let laser = |action, dx, dy| LaserPointer { action, dx, dy };
    phone
        .input
        .laser(pc_id, laser(LaserAction::Show, 0.0, 0.0))
        .unwrap();
    assert_eq!(pc.next().await, Call::Laser(LaserAction::Show, 0.0, 0.0));
    phone
        .input
        .laser(pc_id, laser(LaserAction::Move, 0.25, 7.0))
        .unwrap();
    assert_eq!(pc.next().await, Call::Laser(LaserAction::Move, 0.25, 1.0));
    phone
        .input
        .laser(pc_id, laser(LaserAction::Hide, 0.0, 0.0))
        .unwrap();
    assert_eq!(pc.next().await, Call::Laser(LaserAction::Hide, 0.0, 0.0));
}
