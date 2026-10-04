//! Rooms: who is waiting where, presence updates, and the pipes between matched streams.
//!
//! A connection becomes a *member* of a room with its first listener and stays one until it
//! closes the listener without being called, or disconnects. Membership (not the listener
//! stream) is what the other side sees as presence, so re-arming a listener after each call
//! doesn't flap.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pairly_transport_relay::proto::{self, Room};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::Stats;

type ConnId = u64;

enum ToListener {
    Presence(bool),
    /// A dialer's stream to pipe to.
    Call(quinn::SendStream, quinn::RecvStream),
}

#[derive(Default)]
struct Member {
    /// The waiting listener: its generation and inbox.
    waiting: Option<(u64, mpsc::UnboundedSender<ToListener>)>,
}

#[derive(Default)]
struct State {
    rooms: HashMap<Room, HashMap<ConnId, Member>>,
    by_conn: HashMap<ConnId, HashSet<Room>>,
    next_gen: u64,
}

pub struct Hub {
    state: Mutex<State>,
    rate_limit: Option<u64>,
    stats: Arc<Stats>,
}

impl Hub {
    pub fn new(rate_limit: Option<u64>, stats: Arc<Stats>) -> Self {
        Self {
            state: Mutex::default(),
            rate_limit,
            stats,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait in `room` on `send`/`recv`. Returns false (and closes the stream) if the
    /// connection is in too many rooms.
    pub fn listen(
        self: &Arc<Self>,
        conn: ConnId,
        room: Room,
        mut send: quinn::SendStream,
        recv: quinn::RecvStream,
        max_rooms: usize,
    ) -> bool {
        let (tx, rx) = mpsc::unbounded_channel();
        let generation = {
            let mut st = self.lock();
            let joined = st.by_conn.entry(conn).or_default();
            if !joined.contains(&room) && joined.len() >= max_rooms {
                drop(st);
                tokio::spawn(async move {
                    let _ = send.write_all(&[proto::DENIED]).await;
                    let _ = send.finish();
                });
                return false;
            }
            joined.insert(room);
            st.next_gen += 1;
            let generation = st.next_gen;
            let members = st.rooms.entry(room).or_default();
            let new_member = !members.contains_key(&conn);
            // Replacing an older listener drops its inbox, which ends it.
            members.entry(conn).or_default().waiting = Some((generation, tx.clone()));
            let others = members.len() > 1;
            let _ = tx.send(ToListener::Presence(others));
            if new_member && others {
                notify(members, conn, true);
            }
            generation
        };
        let hub = self.clone();
        tokio::spawn(async move {
            hub.run_listener(conn, room, generation, send, recv, rx)
                .await
        });
        true
    }

    /// Call whoever else waits in `room`, or answer `NO_PEER`.
    pub fn dial(
        &self,
        conn: ConnId,
        room: Room,
        mut send: quinn::SendStream,
        recv: quinn::RecvStream,
    ) {
        let listener = {
            let mut st = self.lock();
            st.rooms.get_mut(&room).and_then(|members| {
                members
                    .iter_mut()
                    .filter(|(id, _)| **id != conn)
                    .find_map(|(_, m)| m.waiting.take())
            })
        };
        let call = ToListener::Call(send, recv);
        let unanswered = match listener {
            Some((_, tx)) => match tx.send(call) {
                Ok(()) => return,
                Err(mpsc::error::SendError(call)) => call,
            },
            None => call,
        };
        if let ToListener::Call(s, _) = unanswered {
            send = s;
            tokio::spawn(async move {
                let _ = send.write_all(&[proto::NO_PEER]).await;
                let _ = send.finish();
            });
        }
    }

    pub fn connection_closed(&self, conn: ConnId) {
        let mut st = self.lock();
        let rooms = st.by_conn.remove(&conn).unwrap_or_default();
        for room in rooms {
            leave(&mut st, conn, room);
        }
    }

    /// Leave `room` if listener `generation` is still the one waiting (the client hung up
    /// without being called).
    fn leave_if_waiting(&self, conn: ConnId, room: Room, generation: u64) {
        let mut st = self.lock();
        let current = st
            .rooms
            .get(&room)
            .and_then(|m| m.get(&conn))
            .and_then(|m| m.waiting.as_ref())
            .is_some_and(|(g, _)| *g == generation);
        if current {
            if let Some(rooms) = st.by_conn.get_mut(&conn) {
                rooms.remove(&room);
            }
            leave(&mut st, conn, room);
        }
    }

    async fn run_listener(
        &self,
        conn: ConnId,
        room: Room,
        generation: u64,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        mut inbox: mpsc::UnboundedReceiver<ToListener>,
    ) {
        let mut probe = [0u8; 1];
        loop {
            tokio::select! {
                msg = inbox.recv() => match msg {
                    Some(ToListener::Presence(present)) => {
                        let signal = if present { proto::PRESENT } else { proto::ABSENT };
                        if send.write_all(&[signal]).await.is_err() {
                            self.leave_if_waiting(conn, room, generation);
                            return;
                        }
                    }
                    Some(ToListener::Call(dial_send, dial_recv)) => {
                        self.pipe(send, recv, dial_send, dial_recv).await;
                        return;
                    }
                    // Replaced by a newer listener from the same device.
                    None => {
                        let _ = send.finish();
                        return;
                    }
                },
                // A waiting listener sends nothing, so this only returns when it hangs up.
                _ = recv.read(&mut probe) => {
                    self.leave_if_waiting(conn, room, generation);
                    return;
                }
            }
        }
    }

    async fn pipe(
        &self,
        mut a_send: quinn::SendStream,
        a_recv: quinn::RecvStream,
        mut b_send: quinn::SendStream,
        b_recv: quinn::RecvStream,
    ) {
        if a_send.write_all(&[proto::MATCHED]).await.is_err()
            || b_send.write_all(&[proto::MATCHED]).await.is_err()
        {
            return;
        }
        self.stats.pipes.fetch_add(1, Ordering::Relaxed);
        self.stats.pipes_total.fetch_add(1, Ordering::Relaxed);
        tokio::join!(
            pump(a_recv, b_send, self.rate_limit, &self.stats),
            pump(b_recv, a_send, self.rate_limit, &self.stats),
        );
        self.stats.pipes.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Tell the other members of a room whether anyone else is there.
fn notify(members: &HashMap<ConnId, Member>, except: ConnId, present: bool) {
    for (id, m) in members {
        if *id != except
            && let Some((_, tx)) = &m.waiting
        {
            let _ = tx.send(ToListener::Presence(present));
        }
    }
}

fn leave(st: &mut State, conn: ConnId, room: Room) {
    let Some(members) = st.rooms.get_mut(&room) else {
        return;
    };
    if members.remove(&conn).is_none() {
        return;
    }
    if members.is_empty() {
        st.rooms.remove(&room);
    } else if members.len() == 1 {
        notify(members, conn, false);
    }
}

/// Copy one direction until it ends, optionally capped at `rate` bytes per second.
async fn pump(
    mut from: quinn::RecvStream,
    mut to: quinn::SendStream,
    rate: Option<u64>,
    stats: &Stats,
) {
    let mut buf = vec![0u8; 64 * 1024];
    let start = Instant::now();
    let mut sent: u64 = 0;
    loop {
        let n = match from.read(&mut buf).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                let _ = to.finish();
                return;
            }
            Err(_) => {
                let _ = to.reset(0u32.into());
                return;
            }
        };
        if to.write_all(&buf[..n]).await.is_err() {
            let _ = from.stop(0u32.into());
            return;
        }
        sent += n as u64;
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(rate) = rate.filter(|r| *r > 0) {
            // Sleep until the average falls back under the cap.
            let due = Duration::from_secs_f64(sent as f64 / rate as f64);
            let elapsed = start.elapsed();
            if due > elapsed {
                tokio::time::sleep(due - elapsed).await;
            }
        }
    }
}
