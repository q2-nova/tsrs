//! The node's keys: `DIR/keys.json`, directory 0700, file 0600, written to a
//! temporary file, synced and renamed. The format is tailscale-rs's own key
//! file (`{"key_state": …}`). Not compatible with tailscaled.state, which this
//! daemon never opens. Key material is never logged: `PersistState`'s Debug
//! output contains it, so nothing here formats a loaded value.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const NAME: &str = "keys.json";
const TEMPORARY: &str = ".keys.json.tmp";
const CORRUPT: &str = "keys.json.corrupt";

#[derive(Serialize, Deserialize)]
struct KeyFile<T> {
    key_state: T,
}

/// What loading found, for the log (fixed words only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Existing,
    Created,
    /// The old file did not parse; it was kept as keys.json.corrupt.
    ReplacedCorrupt,
}

pub struct Loaded<T> {
    pub keys: T,
    pub origin: Origin,
    /// A mode wider than 0600 (file) or 0700 (directory) was narrowed.
    pub mode_fixed: bool,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(NAME)
}

fn private_dir(dir: &Path) -> io::Result<bool> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other("key directory is not a directory"));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        return Ok(true);
    }
    Ok(false)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Write `bytes` as `DIR/keys.json`, mode 0600, replacing it atomically.
fn write_atomic(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = dir.join(TEMPORARY);
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path(dir))?;
    sync_dir(dir)
}

/// Load the keys in `dir`, or create them with `fresh` and store them first.
pub fn load_or_create<T: Serialize + DeserializeOwned>(dir: &Path, fresh: impl FnOnce() -> T) -> io::Result<Loaded<T>> {
    let mut mode_fixed = private_dir(dir)?;
    let file = path(dir);
    let mut origin = Origin::Created;
    match fs::symlink_metadata(&file) {
        Ok(meta) if !meta.is_file() => return Err(io::Error::other("key file is not a regular file")),
        Ok(meta) => {
            if meta.mode() & 0o077 != 0 {
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
                mode_fixed = true;
            }
            match serde_json::from_slice::<KeyFile<T>>(&fs::read(&file)?) {
                Ok(KeyFile { key_state }) => return Ok(Loaded { keys: key_state, origin: Origin::Existing, mode_fixed }),
                Err(_) => {
                    fs::rename(&file, dir.join(CORRUPT))?;
                    origin = Origin::ReplacedCorrupt;
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let keys = fresh();
    let bytes = serde_json::to_vec(&KeyFile { key_state: &keys }).map_err(io::Error::other)?;
    write_atomic(dir, &bytes)?;
    Ok(Loaded { keys, origin, mode_fixed })
}

/// Forget this node: remove its key file. Whether there was one.
pub fn remove(dir: &Path) -> io::Result<bool> {
    match fs::remove_file(path(dir)) {
        Ok(()) => {
            sync_dir(dir)?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/tsrsd/test").join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn created_private_then_loaded_unchanged() {
        let dir = scratch("keys-create").join("tailscale-rs");
        let first = load_or_create(&dir, || vec![1u8, 2, 3]).unwrap();
        assert_eq!((first.origin, first.mode_fixed), (Origin::Created, false));
        assert_eq!((mode(&dir), mode(&path(&dir))), (0o700, 0o600));
        assert!(!dir.join(TEMPORARY).exists());
        let again = load_or_create(&dir, || -> Vec<u8> { panic!("existing keys must be used") }).unwrap();
        assert_eq!((again.keys, again.origin), (vec![1, 2, 3], Origin::Existing));
        assert_eq!(fs::read_to_string(path(&dir)).unwrap(), r#"{"key_state":[1,2,3]}"#);
        assert!(remove(&dir).unwrap());
        assert!(!remove(&dir).unwrap());
        assert_eq!(load_or_create(&dir, || vec![9u8]).unwrap().origin, Origin::Created);
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn wide_modes_are_narrowed_and_corrupt_files_kept_aside() {
        let dir = scratch("keys-modes");
        load_or_create(&dir, || 7u32).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(path(&dir), fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = load_or_create(&dir, || 8u32).unwrap();
        assert_eq!((loaded.keys, loaded.mode_fixed), (7, true));
        assert_eq!((mode(&dir), mode(&path(&dir))), (0o700, 0o600));
        fs::write(path(&dir), b"not json").unwrap();
        let replaced = load_or_create(&dir, || 9u32).unwrap();
        assert_eq!((replaced.keys, replaced.origin), (9, Origin::ReplacedCorrupt));
        assert_eq!(fs::read(dir.join(CORRUPT)).unwrap(), b"not json");
        // A stale temporary file from an interrupted write does not block the next one.
        fs::write(dir.join(TEMPORARY), b"x").unwrap();
        remove(&dir).unwrap();
        load_or_create(&dir, || 10u32).unwrap();
        assert!(!dir.join(TEMPORARY).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_key_file_is_refused() {
        let dir = scratch("keys-link");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/dev/null", path(&dir)).unwrap();
        assert!(load_or_create(&dir, || 1u8).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
