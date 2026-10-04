use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zeroize::Zeroizing;

use crate::{CryptoError, IdentityKeypair, KEY_LEN};

/// Where the identity secret lives. Linux uses [`FileKeyStore`] (Secret Service later); Android
/// implements this over a Keystore-wrapped blob via the FFI layer.
pub trait KeyStore: Send + Sync {
    fn load(&self) -> Result<Option<IdentityKeypair>, CryptoError>;
    fn store(&self, keypair: &IdentityKeypair) -> Result<(), CryptoError>;
}

/// Load the identity, generating and storing a fresh one on first run.
pub fn load_or_generate(store: &dyn KeyStore) -> Result<IdentityKeypair, CryptoError> {
    if let Some(kp) = store.load()? {
        return Ok(kp);
    }
    let kp = IdentityKeypair::generate();
    store.store(&kp)?;
    Ok(kp)
}

const FILE_MAGIC: &[u8; 4] = b"PLY1";

/// Secret stored as `PLY1 || secret` in a `0600` file, written atomically.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn io_err(e: std::io::Error) -> CryptoError {
    CryptoError::KeyStore(e.to_string())
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<IdentityKeypair>, CryptoError> {
        let data = match fs::read(&self.path) {
            Ok(d) => Zeroizing::new(d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(e)),
        };
        let secret: [u8; KEY_LEN] = data
            .strip_prefix(FILE_MAGIC)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| CryptoError::KeyStore("identity file is corrupt".into()))?;
        Ok(Some(IdentityKeypair::from_secret(secret)))
    }

    fn store(&self, keypair: &IdentityKeypair) -> Result<(), CryptoError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let tmp = self.path.with_extension("tmp");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp).map_err(io_err)?;
        let mut data = Zeroizing::new(Vec::with_capacity(4 + KEY_LEN));
        data.extend_from_slice(FILE_MAGIC);
        data.extend_from_slice(keypair.secret_bytes());
        f.write_all(&data).map_err(io_err)?;
        f.sync_all().map_err(io_err)?;
        fs::rename(&tmp, &self.path).map_err(io_err)
    }
}

/// In-memory store for tests and ephemeral nodes.
#[derive(Default)]
pub struct MemoryKeyStore(Mutex<Option<IdentityKeypair>>);

impl MemoryKeyStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Option<IdentityKeypair>, CryptoError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| CryptoError::KeyStore("poisoned".into()))?
            .clone())
    }

    fn store(&self, keypair: &IdentityKeypair) -> Result<(), CryptoError> {
        *self
            .0
            .lock()
            .map_err(|_| CryptoError::KeyStore("poisoned".into()))? = Some(keypair.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(dir.path().join("sub/identity.key"));
        assert!(store.load().unwrap().is_none());
        let kp = load_or_generate(&store).unwrap();
        assert_eq!(load_or_generate(&store).unwrap().public(), kp.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, b"garbage").unwrap();
        assert!(FileKeyStore::new(path).load().is_err());
    }

    #[test]
    fn memory_store() {
        let store = MemoryKeyStore::new();
        let kp = load_or_generate(&store).unwrap();
        assert_eq!(store.load().unwrap().unwrap().public(), kp.public());
    }
}
