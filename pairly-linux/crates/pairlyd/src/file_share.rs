//! Sharing a folder of this PC with paired devices, so they can browse it and download from it
//! wherever they are. Off until the user turns it on; read-only unless they allow changes;
//! hidden files (`.ssh`, `.config`…) are never shared.

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};

use pairly_plugins::files::FilesHost;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sharing {
    pub on: bool,
    pub folder: PathBuf,
    /// Paired devices may upload, rename and delete (else only look and download).
    pub allow_changes: bool,
}

/// The current settings; changed from the app without restarting.
pub struct FileShare(RwLock<Sharing>);

impl FileShare {
    pub fn new(sharing: Sharing) -> Self {
        Self(RwLock::new(sharing))
    }

    pub fn get(&self) -> Sharing {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, sharing: Sharing) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = sharing;
    }
}

impl FilesHost for FileShare {
    fn root(&self) -> Result<PathBuf, String> {
        let sharing = self.get();
        if !sharing.on {
            return Err(
                "this PC doesn't share its files (turn it on in Pairly's settings there)".into(),
            );
        }
        if !sharing.folder.is_dir() {
            return Err(format!(
                "the shared folder {} is gone",
                sharing.folder.display()
            ));
        }
        Ok(sharing.folder)
    }

    fn allow_changes(&self) -> bool {
        self.get().allow_changes
    }

    fn show_hidden(&self) -> bool {
        false
    }
}
