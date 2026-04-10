//! File-based JSON state store with flock-based locking.
//!
//! Provides atomic reads and writes of JSON-serialized state files.
//! Shared locks (`LOCK_SH`) for concurrent readers, exclusive locks
//! (`LOCK_EX`) for writers. Writes use temp file + `rename()` for
//! atomicity — readers never see partial data.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{Error, Result};

/// File-based JSON state store rooted at a directory.
///
/// The directory layout is:
/// ```text
/// <root>/
/// ├── config.json
/// ├── kernels/
/// ├── images/
/// │   └── registry.json
/// ├── vms/
/// │   └── <vm-name>/
/// │       ├── vm.json
/// │       ├── vsock.sock
/// │       ├── firecracker.sock
/// │       ├── firecracker.log
/// │       ├── console.log
/// │       └── firecracker.pid
/// ├── vsock/
/// │   └── cids.json
/// └── network/
///     └── allocations.json
/// ```
#[derive(Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    /// Create a new state store backed by the given directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Open an existing state store, returning `None` if the directory
    /// doesn't exist (e.g., before `ember init` has been run).
    pub fn try_open(root: &Path) -> Option<Self> {
        if root.join("vms").is_dir() {
            Some(Self {
                root: root.to_path_buf(),
            })
        } else {
            None
        }
    }

    /// Initialize the state directory structure.
    ///
    /// Creates the root and all standard subdirectories if they don't exist.
    pub fn init(&self) -> Result<()> {
        let dirs = [
            self.root.clone(),
            self.kernel_dir(),
            self.root.join("images"),
            self.root.join("vms"),
            self.root.join("vsock"),
            self.root.join("network"),
        ];
        for dir in &dirs {
            fs::create_dir_all(dir).map_err(|e| Error::Io {
                path: dir.clone(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Root directory of this state store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for a specific VM's state files.
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.root.join("vms").join(name)
    }

    /// Path to a VM's metadata file.
    pub fn vm_metadata_path(&self, name: &str) -> PathBuf {
        self.vm_dir(name).join("vm.json")
    }

    /// Path to the local image registry file.
    pub fn image_registry_path(&self) -> PathBuf {
        self.root.join("images").join("registry.json")
    }

    /// Path to network IP allocation tracking.
    pub fn network_allocations_path(&self) -> PathBuf {
        self.root.join("network").join("allocations.json")
    }

    /// Path to vsock CID allocation tracking.
    pub fn vsock_allocations_path(&self) -> PathBuf {
        self.root.join("vsock").join("cids.json")
    }

    /// Path to the global config file.
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Directory for kernel binaries.
    pub fn kernel_dir(&self) -> PathBuf {
        self.root.join("kernels")
    }

    /// Read and deserialize a JSON file, using a shared (read) lock.
    ///
    /// Returns an error if the file does not exist or cannot be parsed.
    pub fn read<T: DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let _lock = FileLock::shared(path)?;

        let contents = fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;

        serde_json::from_str(&contents).map_err(Into::into)
    }

    /// Read and deserialize a JSON file, returning `None` if it doesn't exist.
    ///
    /// Uses a shared (read) lock. Returns an error only on I/O failures
    /// other than "not found" or on parse errors.
    pub fn read_optional<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        self.read(path).map(Some)
    }

    /// Serialize and write a JSON file atomically, using an exclusive lock.
    ///
    /// Writes to a temporary file first, then renames to the target path.
    /// Parent directories are created if needed.
    pub fn write<T: Serialize>(&self, path: &Path, data: &T) -> Result<()> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let _lock = FileLock::exclusive(path)?;

        // Write to temp file in the same directory (same filesystem for rename).
        let tmp_path = tmp_path_for(path);
        let json = serde_json::to_string_pretty(data)?;

        fs::write(&tmp_path, json.as_bytes()).map_err(|e| Error::Io {
            path: tmp_path.clone(),
            source: e,
        })?;

        // Atomic rename.
        fs::rename(&tmp_path, path).map_err(|e| {
            // Best-effort cleanup of temp file on rename failure.
            let _ = fs::remove_file(&tmp_path);
            Error::Io {
                path: path.to_path_buf(),
                source: e,
            }
        })?;

        Ok(())
    }

    /// Remove a file, ignoring "not found" errors.
    pub fn remove(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Remove a directory and all its contents, ignoring "not found" errors.
    pub fn remove_dir(&self, path: &Path) -> Result<()> {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }
}

/// Generate a temporary file path adjacent to `path`.
///
/// Includes the PID to avoid collisions between concurrent processes.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(tmp)
}

/// Companion `.lock` file path for a data file.
///
/// Using a separate lock file avoids inode-replacement issues with
/// atomic rename writes while still providing correct flock semantics.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// RAII guard that holds an `flock` on a companion `.lock` file.
///
/// The lock is released automatically when the inner `Flock` is dropped.
struct FileLock {
    _flock: Flock<File>,
}

impl FileLock {
    /// Acquire a shared (read) lock for the given data file.
    ///
    /// Opens the lock file read-only without creating it. If the lock file
    /// doesn't exist (e.g., no writer has ever run, or we lack permission
    /// to create it), returns `None` — reads proceed without locking, which
    /// is safe because the data file is updated via atomic rename.
    fn shared(data_path: &Path) -> Result<Option<Self>> {
        let lock_path = lock_path_for(data_path);

        let file = match OpenOptions::new().read(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
            Err(e) => {
                return Err(Error::Io {
                    path: lock_path,
                    source: e,
                })
            }
        };

        let flock = Flock::lock(file, FlockArg::LockShared).map_err(|(_, errno)| Error::Io {
            path: lock_path,
            source: errno.into(),
        })?;

        Ok(Some(Self { _flock: flock }))
    }

    /// Acquire an exclusive (write) lock for the given data file.
    fn exclusive(data_path: &Path) -> Result<Self> {
        let lock_path = lock_path_for(data_path);

        // Ensure parent directory exists for the lock file.
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        let flock = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| Error::Io {
            path: lock_path,
            source: errno.into(),
        })?;

        Ok(Self { _flock: flock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn tmp_path_contains_pid() {
        let path = Path::new("/var/lib/ember/config.json");
        let tmp = tmp_path_for(path);
        let tmp_str = tmp.to_string_lossy();
        assert!(tmp_str.starts_with("/var/lib/ember/config.json.tmp."));
        assert!(tmp_str.contains(&std::process::id().to_string()));
    }

    #[test]
    fn lock_path_has_lock_extension() {
        let path = Path::new("/var/lib/ember/config.json");
        let lock = lock_path_for(path);
        assert_eq!(lock, PathBuf::from("/var/lib/ember/config.json.lock"));
    }

    #[test]
    fn round_trip_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let data: HashMap<String, String> = [("key".to_string(), "value".to_string())].into();

        let path = dir.path().join("test.json");
        store.write(&path, &data).unwrap();

        let loaded: HashMap<String, String> = store.read(&path).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn read_optional_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let path = dir.path().join("nonexistent.json");
        let result: Option<HashMap<String, String>> = store.read_optional(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_optional_returns_some_for_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let data = vec![1u32, 2, 3];
        let path = dir.path().join("list.json");
        store.write(&path, &data).unwrap();

        let loaded: Option<Vec<u32>> = store.read_optional(&path).unwrap();
        assert_eq!(loaded, Some(vec![1, 2, 3]));
    }

    #[test]
    fn init_creates_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let store = StateStore::new(root.clone());
        store.init().unwrap();

        assert!(root.join("kernels").is_dir());
        assert!(root.join("images").is_dir());
        assert!(root.join("vms").is_dir());
        assert!(root.join("vsock").is_dir());
        assert!(root.join("network").is_dir());
    }

    #[test]
    fn remove_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let path = dir.path().join("gone.json");
        // Removing a nonexistent file should succeed.
        store.remove(&path).unwrap();

        // Write then remove.
        store.write(&path, &"hello").unwrap();
        assert!(path.exists());
        store.remove(&path).unwrap();
        assert!(!path.exists());
        // Removing again should still succeed.
        store.remove(&path).unwrap();
    }

    #[test]
    fn remove_dir_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let vm_dir = dir.path().join("vms").join("testvm");
        fs::create_dir_all(&vm_dir).unwrap();
        fs::write(vm_dir.join("vm.json"), "{}").unwrap();

        store.remove_dir(&vm_dir).unwrap();
        assert!(!vm_dir.exists());
        // Second call should not error.
        store.remove_dir(&vm_dir).unwrap();
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());

        let path = dir.path().join("deep").join("nested").join("file.json");
        store.write(&path, &42u32).unwrap();

        let loaded: u32 = store.read(&path).unwrap();
        assert_eq!(loaded, 42);
    }

    #[test]
    fn path_helpers() {
        let store = StateStore::new(PathBuf::from("/var/lib/ember"));

        assert_eq!(
            store.vm_dir("myvm"),
            PathBuf::from("/var/lib/ember/vms/myvm")
        );
        assert_eq!(
            store.vm_metadata_path("myvm"),
            PathBuf::from("/var/lib/ember/vms/myvm/vm.json")
        );
        assert_eq!(
            store.image_registry_path(),
            PathBuf::from("/var/lib/ember/images/registry.json")
        );
        assert_eq!(
            store.network_allocations_path(),
            PathBuf::from("/var/lib/ember/network/allocations.json")
        );
        assert_eq!(
            store.vsock_allocations_path(),
            PathBuf::from("/var/lib/ember/vsock/cids.json")
        );
        assert_eq!(
            store.config_path(),
            PathBuf::from("/var/lib/ember/config.json")
        );
        assert_eq!(store.kernel_dir(), PathBuf::from("/var/lib/ember/kernels"));
    }
}
