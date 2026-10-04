//! File browsing: whatever path a PC sends, it must resolve inside the phone's shared folder,
//! even with `..`, absolute paths, backslashes or a symlink that points outside.
#![no_main]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use pairly_plugins::files::resolve;

/// `<tmp>/root` (shared) with `dir/`, `file`, and `escape` → `<tmp>/outside`.
fn root() -> &'static (PathBuf, PathBuf) {
    static ROOT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("pairly-fuzz-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        std::fs::write(outside.join("secret"), b"x").unwrap();
        let _ = std::os::unix::fs::symlink(&outside, root.join("escape"));
        (root.canonicalize().unwrap(), outside.canonicalize().unwrap())
    })
}

fn check(root: &Path, outside: &Path, rel: &str, must_exist: bool) {
    if let Ok(path) = resolve(root, rel, must_exist) {
        assert!(path.starts_with(root), "{rel:?} resolved to {path:?}");
        assert!(!path.starts_with(outside), "{rel:?} escaped to {path:?}");
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(rel) = std::str::from_utf8(data) else {
        return;
    };
    let (root, outside) = root();
    check(root, outside, rel, true);
    check(root, outside, rel, false);
});
