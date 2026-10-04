//! Paired devices, persisted in SQLite. (Encrypted with SQLCipher in Phase 11.)

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use pairly_crypto::{DeviceId, PublicKey};
use pairly_proto::packets::DeviceType;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::Result;

const SCHEMA_VERSION: i32 = 2;

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
}

impl fmt::Debug for PairedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedDevice")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("device_type", &self.device_type)
            .field("paired_at", &self.paired_at)
            .field("relay", &self.relay)
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
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Insert or replace a pairing (re-pairing overwrites the secret).
    pub fn upsert(&self, d: &PairedDevice) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO devices
                (id, public_key, name, device_type, pair_secret, paired_at, relay)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                d.id.to_string(),
                &d.public_key.as_bytes()[..],
                d.name,
                d.device_type.as_str(),
                &d.pair_secret[..],
                i64::try_from(d.paired_at).unwrap_or(i64::MAX),
                d.relay,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &DeviceId) -> Result<Option<PairedDevice>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT * FROM devices WHERE id = ?1")?;
        Ok(stmt.query_row([id.to_string()], from_row).optional()?)
    }

    /// Whether `key` belongs to a paired device (the id is derived from the key, so both match).
    pub fn is_paired_key(&self, key: &PublicKey) -> Result<bool> {
        Ok(self
            .get(&key.device_id())?
            .is_some_and(|d| d.public_key == *key))
    }

    pub fn list(&self) -> Result<Vec<PairedDevice>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT * FROM devices ORDER BY paired_at")?;
        let rows = stmt.query_map([], from_row)?;
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
        Ok(self.lock().execute(
            "UPDATE devices SET relay = ?2 WHERE id = ?1 AND relay IS NOT ?2",
            params![id.to_string(), relay],
        )? > 0)
    }

    /// Returns whether a device was removed.
    pub fn remove(&self, id: &DeviceId) -> Result<bool> {
        Ok(self
            .lock()
            .execute("DELETE FROM devices WHERE id = ?1", [id.to_string()])?
            > 0)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<PairedDevice> {
    fn bad(col: usize, what: &str) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, what.into())
    }
    let id: String = row.get("id")?;
    let key: Vec<u8> = row.get("public_key")?;
    let device_type: String = row.get("device_type")?;
    let secret: Vec<u8> = row.get("pair_secret")?;
    let paired_at: i64 = row.get("paired_at")?;
    Ok(PairedDevice {
        id: id.parse().map_err(|_| bad(0, "bad device id"))?,
        public_key: PublicKey::from_slice(&key).map_err(|_| bad(1, "bad public key"))?,
        name: row.get("name")?,
        device_type: DeviceType::parse(&device_type).ok_or_else(|| bad(3, "bad device type"))?,
        pair_secret: secret.try_into().map_err(|_| bad(4, "bad pair secret"))?,
        paired_at: u64::try_from(paired_at).unwrap_or(0),
        relay: row.get("relay")?,
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
        assert_eq!((got.name.as_str(), got.relay), ("Old", None));
        assert!(reg.set_relay(&d.id, Some("pairly-relay://r:1/p")).unwrap());
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
