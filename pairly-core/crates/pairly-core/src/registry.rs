//! Paired devices, persisted in SQLite.
//!
//! The secret fields (each pairing's `pair_secret`, and the relay address, which carries the
//! relay's access token) are encrypted once the node calls [`Registry::seal_with`] with a key
//! derived from its identity. Names, public keys and Bluetooth addresses aren't secret and stay
//! readable.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use pairly_crypto::{DeviceId, FieldKey, PublicKey};
use pairly_proto::packets::DeviceType;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{CoreError, Result};

const SCHEMA_VERSION: i32 = 3;
/// The same schema, with the secret fields sealed.
const SEALED_VERSION: i32 = 4;

#[derive(Clone)]
pub struct PairedDevice {
    pub id: DeviceId,
    pub public_key: PublicKey,
    pub name: String,
    pub device_type: DeviceType,
    /// Shared secret from pairing; used later to derive the relay room.
    pub pair_secret: [u8; 32],
    /// Unix seconds.
    pub paired_at: u64,
    /// The relay the device last told us it uses (`pairly-relay://…`).
    pub relay: Option<String>,
    /// The device's Bluetooth address: announced by it, or seen on a connection from it.
    pub bluetooth: Option<String>,
    /// Paused by the user: still paired, but no connection either way until resumed.
    pub paused: bool,
}

impl fmt::Debug for PairedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedDevice")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("device_type", &self.device_type)
            .field("paired_at", &self.paired_at)
            // The relay address carries its access token.
            .field("relay", &self.relay.as_ref().map(|_| "…"))
            .field("bluetooth", &self.bluetooth)
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct Registry {
    conn: Mutex<Connection>,
    /// The stored secrets are sealed (schema version 4).
    sealed: AtomicBool,
    /// Set by [`Self::seal_with`].
    key: OnceLock<FieldKey>,
}

fn aad(field: &str, id: &str) -> Vec<u8> {
    format!("pairly-registry/{field}/{id}").into_bytes()
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE devices (
                    id          TEXT PRIMARY KEY,
                    public_key  BLOB NOT NULL UNIQUE,
                    name        TEXT NOT NULL,
                    device_type TEXT NOT NULL,
                    pair_secret BLOB NOT NULL,
                    paired_at   INTEGER NOT NULL
                );",
            )?;
        }
        if version < 2 {
            conn.execute_batch("ALTER TABLE devices ADD COLUMN relay TEXT;")?;
        }
        if version < 3 {
            conn.execute_batch("ALTER TABLE devices ADD COLUMN bluetooth TEXT;")?;
        }
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // Added after the sealed schema (version 4), so checked by the column itself.
        if conn.prepare("SELECT paused FROM devices LIMIT 0").is_err() {
            conn.execute_batch(
                "ALTER TABLE devices ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            sealed: AtomicBool::new(version >= SEALED_VERSION),
            key: OnceLock::new(),
        })
    }

    /// Encrypt the secret fields with `key` from now on, sealing any stored in plaintext by an
    /// older version (once, in one transaction). The node calls this right after loading its
    /// identity, before using the registry.
    pub fn seal_with(&self, key: FieldKey) -> Result<()> {
        let conn = self.lock();
        if !self.sealed.load(Ordering::Acquire) {
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<(String, Vec<u8>, Option<String>)> = {
                let mut stmt = tx.prepare("SELECT id, pair_secret, relay FROM devices")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (id, secret, relay) in &rows {
                tx.execute(
                    "UPDATE devices SET pair_secret = ?2, relay = ?3 WHERE id = ?1",
                    params![
                        id,
                        key.seal(&aad("pair_secret", id), secret),
                        relay
                            .as_ref()
                            .map(|r| key.seal(&aad("relay", id), r.as_bytes())),
                    ],
                )?;
            }
            tx.pragma_update(None, "user_version", SEALED_VERSION)?;
            tx.commit()?;
            self.sealed.store(true, Ordering::Release);
            if !rows.is_empty() {
                tracing::info!(devices = rows.len(), "sealed the stored pairing secrets");
            }
        }
        // A second call (another node on the same registry) keeps the first key.
        let _ = self.key.set(key);
        Ok(())
    }

    /// The key for sealed fields: `None` while they're stored in plaintext.
    fn key(&self) -> Result<Option<&FieldKey>> {
        match (self.sealed.load(Ordering::Acquire), self.key.get()) {
            (false, _) => Ok(None),
            (true, Some(key)) => Ok(Some(key)),
            (true, None) => Err(CoreError::Violation(
                "the registry's secrets are sealed and no key was given",
            )),
        }
    }

    fn seal_secret(&self, id: &str, secret: &[u8; 32]) -> Result<Vec<u8>> {
        Ok(match self.key()? {
            Some(key) => key.seal(&aad("pair_secret", id), secret),
            None => secret.to_vec(),
        })
    }

    fn seal_relay(&self, id: &str, relay: Option<&str>) -> Result<Value> {
        Ok(match (relay, self.key()?) {
            (None, _) => Value::Null,
            (Some(r), Some(key)) => Value::Blob(key.seal(&aad("relay", id), r.as_bytes())),
            (Some(r), None) => Value::Text(r.to_owned()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Insert or replace a pairing (re-pairing overwrites the secret).
    pub fn upsert(&self, d: &PairedDevice) -> Result<()> {
        let id = d.id.to_string();
        let secret = self.seal_secret(&id, &d.pair_secret)?;
        let relay = self.seal_relay(&id, d.relay.as_deref())?;
        self.lock().execute(
            "INSERT OR REPLACE INTO devices
                (id, public_key, name, device_type, pair_secret, paired_at, relay, bluetooth,
                 paused)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                &d.public_key.as_bytes()[..],
                d.name,
                d.device_type.as_str(),
                secret,
                i64::try_from(d.paired_at).unwrap_or(i64::MAX),
                relay,
                d.bluetooth,
                d.paused,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &DeviceId) -> Result<Option<PairedDevice>> {
        let key = self.key()?;
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT * FROM devices WHERE id = ?1")?;
        Ok(stmt
            .query_row([id.to_string()], |r| from_row(r, key))
            .optional()?)
    }

    /// Whether `key` belongs to a paired device (the id is derived from the key, so both match).
    pub fn is_paired_key(&self, key: &PublicKey) -> Result<bool> {
        Ok(self
            .get(&key.device_id())?
            .is_some_and(|d| d.public_key == *key))
    }

    /// Paired with this key and not paused: may connect.
    pub fn is_active_key(&self, key: &PublicKey) -> Result<bool> {
        Ok(self
            .get(&key.device_id())?
            .is_some_and(|d| d.public_key == *key && !d.paused))
    }

    /// Pause or resume a paired device. Returns whether it changed.
    pub fn set_paused(&self, id: &DeviceId, paused: bool) -> Result<bool> {
        Ok(self.lock().execute(
            "UPDATE devices SET paused = ?2 WHERE id = ?1 AND paused != ?2",
            params![id.to_string(), paused],
        )? > 0)
    }

    pub fn list(&self) -> Result<Vec<PairedDevice>> {
        let key = self.key()?;
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT * FROM devices ORDER BY paired_at")?;
        let rows = stmt.query_map([], |r| from_row(r, key))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_name(&self, id: &DeviceId, name: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE devices SET name = ?2 WHERE id = ?1",
            params![id.to_string(), name],
        )?;
        Ok(())
    }

    /// Remember the relay a device announced. Returns whether it changed.
    pub fn set_relay(&self, id: &DeviceId, relay: Option<&str>) -> Result<bool> {
        // Sealed values differ every time, so compare the plaintext.
        match self.get(id)? {
            Some(d) if d.relay.as_deref() != relay => {}
            _ => return Ok(false),
        }
        let id = id.to_string();
        let value = self.seal_relay(&id, relay)?;
        Ok(self.lock().execute(
            "UPDATE devices SET relay = ?2 WHERE id = ?1",
            params![id, value],
        )? > 0)
    }

    /// Remember a device's Bluetooth address. Returns whether it changed.
    pub fn set_bluetooth(&self, id: &DeviceId, address: &str) -> Result<bool> {
        Ok(self.lock().execute(
            "UPDATE devices SET bluetooth = ?2 WHERE id = ?1 AND bluetooth IS NOT ?2",
            params![id.to_string(), address.to_ascii_uppercase()],
        )? > 0)
    }

    /// How many devices are paired (readable without the key).
    pub fn device_count(&self) -> Result<usize> {
        let n: i64 = self
            .lock()
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Remove pairings whose secrets this key can't open (made under a previous identity, so
    /// the other device pinned a key we no longer have). Returns how many.
    pub fn drop_unreadable(&self) -> Result<usize> {
        let ids: Vec<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT id FROM devices")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut dropped = 0;
        for id in ids {
            let readable = match id.parse::<DeviceId>() {
                Ok(device) => self.get(&device).is_ok(),
                Err(_) => false,
            };
            if !readable {
                self.lock()
                    .execute("DELETE FROM devices WHERE id = ?1", [&id])?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// Returns whether a device was removed.
    pub fn remove(&self, id: &DeviceId) -> Result<bool> {
        Ok(self
            .lock()
            .execute("DELETE FROM devices WHERE id = ?1", [id.to_string()])?
            > 0)
    }
}

/// One device row; `key` opens the sealed fields (`None` while they're plaintext).
fn from_row(row: &Row<'_>, key: Option<&FieldKey>) -> rusqlite::Result<PairedDevice> {
    fn bad(col: usize, what: &str) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, what.into())
    }
    let id: String = row.get("id")?;
    let public_key: Vec<u8> = row.get("public_key")?;
    let device_type: String = row.get("device_type")?;
    let mut secret: Vec<u8> = row.get("pair_secret")?;
    if let Some(key) = key {
        secret = key
            .open(&aad("pair_secret", &id), &secret)
            .map_err(|_| bad(4, "pair secret doesn't open"))?;
    }
    let relay = match (row.get::<_, Value>("relay")?, key) {
        (Value::Null, _) => None,
        (Value::Text(r), _) => Some(r),
        (Value::Blob(sealed), Some(key)) => Some(
            key.open(&aad("relay", &id), &sealed)
                .ok()
                .and_then(|r| String::from_utf8(r).ok())
                .ok_or_else(|| bad(6, "relay doesn't open"))?,
        ),
        _ => return Err(bad(6, "bad relay")),
    };
    let paired_at: i64 = row.get("paired_at")?;
    Ok(PairedDevice {
        id: id.parse().map_err(|_| bad(0, "bad device id"))?,
        public_key: PublicKey::from_slice(&public_key).map_err(|_| bad(1, "bad public key"))?,
        name: row.get("name")?,
        device_type: DeviceType::parse(&device_type).ok_or_else(|| bad(3, "bad device type"))?,
        pair_secret: secret.try_into().map_err(|_| bad(4, "bad pair secret"))?,
        paired_at: u64::try_from(paired_at).unwrap_or(0),
        relay,
        bluetooth: row.get("bluetooth")?,
        paused: row.get("paused")?,
    })
}

#[cfg(test)]
mod tests {
    use pairly_crypto::IdentityKeypair;

    use super::*;

    fn device(name: &str) -> PairedDevice {
        let key = IdentityKeypair::generate().public();
        PairedDevice {
            id: key.device_id(),
            public_key: key,
            name: name.into(),
            device_type: DeviceType::Phone,
            pair_secret: [3; 32],
            paired_at: unix_now(),
            relay: None,
            bluetooth: None,
            paused: false,
        }
    }

    #[test]
    fn crud() {
        let reg = Registry::open_in_memory().unwrap();
        let d = device("Phone");
        reg.upsert(&d).unwrap();
        let got = reg.get(&d.id).unwrap().unwrap();
        assert_eq!(
            (got.public_key, got.name.as_str(), got.pair_secret),
            (d.public_key, "Phone", [3; 32])
        );
        assert!(reg.is_paired_key(&d.public_key).unwrap());
        assert!(
            !reg.is_paired_key(&IdentityKeypair::generate().public())
                .unwrap()
        );

        assert!(reg.set_relay(&d.id, Some("pairly-relay://r:1/p")).unwrap());
        assert!(!reg.set_relay(&d.id, Some("pairly-relay://r:1/p")).unwrap());
        assert_eq!(
            reg.get(&d.id).unwrap().unwrap().relay.as_deref(),
            Some("pairly-relay://r:1/p")
        );

        assert!(reg.set_bluetooth(&d.id, "5c:ba:ef:42:73:8c").unwrap());
        assert!(!reg.set_bluetooth(&d.id, "5C:BA:EF:42:73:8C").unwrap());

        reg.set_name(&d.id, "Renamed").unwrap();
        assert_eq!(reg.list().unwrap()[0].name, "Renamed");
        assert!(reg.remove(&d.id).unwrap());
        assert!(!reg.remove(&d.id).unwrap());
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn upgrades_a_version_1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        let d = device("Old");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE devices (id TEXT PRIMARY KEY, public_key BLOB NOT NULL UNIQUE,
                    name TEXT NOT NULL, device_type TEXT NOT NULL, pair_secret BLOB NOT NULL,
                    paired_at INTEGER NOT NULL);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO devices VALUES (?1, ?2, 'Old', 'phone', ?3, 1)",
                params![
                    d.id.to_string(),
                    &d.public_key.as_bytes()[..],
                    &[3u8; 32][..]
                ],
            )
            .unwrap();
        }
        let reg = Registry::open(&path).unwrap();
        let got = reg.get(&d.id).unwrap().unwrap();
        assert_eq!(
            (got.name.as_str(), got.relay, got.bluetooth),
            ("Old", None, None)
        );
        assert!(reg.set_relay(&d.id, Some("pairly-relay://r:1/p")).unwrap());
    }

    #[test]
    fn secrets_are_sealed_at_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        let identity = IdentityKeypair::generate();
        let mut old = device("Phone");
        old.pair_secret = [0xAB; 32];
        old.relay = Some("pairly-relay://SECRET-TOKEN@r:1/p".into());
        // Stored by a version that didn't seal.
        Registry::open(&path).unwrap().upsert(&old).unwrap();

        let reg = Registry::open(&path).unwrap();
        reg.seal_with(FieldKey::derive(&identity, "registry"))
            .unwrap();
        let mut new = device("Laptop");
        new.pair_secret = [0xCD; 32];
        reg.upsert(&new).unwrap();
        let got = reg.get(&old.id).unwrap().unwrap();
        assert_eq!(
            (got.pair_secret, got.relay.clone()),
            (old.pair_secret, old.relay.clone())
        );
        assert!(
            !reg.set_relay(&old.id, old.relay.as_deref()).unwrap(),
            "unchanged"
        );
        drop(reg);

        // Neither secret is anywhere in the file any more.
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .unwrap();
        drop(conn);
        let bytes = std::fs::read(&path).unwrap();
        for needle in [&[0xAB; 32][..], &[0xCD; 32][..], b"SECRET-TOKEN"] {
            assert!(!bytes.windows(needle.len()).any(|w| w == needle));
        }

        // The right key reads them back; a missing or wrong one doesn't.
        let reg = Registry::open(&path).unwrap();
        assert!(reg.list().is_err(), "sealed, no key yet");
        reg.seal_with(FieldKey::derive(&identity, "registry"))
            .unwrap();
        assert_eq!(reg.get(&new.id).unwrap().unwrap().pair_secret, [0xCD; 32]);
        let reg = Registry::open(&path).unwrap();
        reg.seal_with(FieldKey::derive(&IdentityKeypair::generate(), "registry"))
            .unwrap();
        assert!(reg.get(&old.id).is_err());
        // A new identity can't use them: they're cleared, and the registry works again.
        assert_eq!(reg.drop_unreadable().unwrap(), 2);
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        let d = device("Laptop");
        Registry::open(&path).unwrap().upsert(&d).unwrap();
        let reg = Registry::open(&path).unwrap();
        assert_eq!(reg.get(&d.id).unwrap().unwrap().name, "Laptop");
    }
}
