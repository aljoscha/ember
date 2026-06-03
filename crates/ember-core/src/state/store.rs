//! File-based JSON state store with flock-based locking.
//!
//! Provides atomic reads and writes of JSON-serialized state files.
//! Shared locks (`LOCK_SH`) for concurrent readers, exclusive locks
//! (`LOCK_EX`) for writers. Writes use temp file + `rename()` for
//! atomicity — readers never see partial data.
//!
//! Mutations go through [`StateStore::update`] / [`StateStore::update_with`]
//! (read-modify-write) or [`StateStore::create`] (write-once). These hold a
//! single exclusive lock across the whole transaction, so concurrent
//! processes cannot lose each other's updates. There is deliberately no
//! fire-and-forget `write`: an unlocked read-then-write would reopen the
//! lost-update window these methods exist to close.

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
/// │       ├── firecracker.sock
/// │       ├── firecracker.log
/// │       ├── console.log
/// │       └── firecracker.pid
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
            self.root.join("network"),
        ];
        for dir in &dirs {
            fs::create_dir_all(dir).map_err(|e| Error::Io {
                path: dir.clone(),
                source: e,
            })?;
        }

        // Pre-create companion lock files for the shared, append-heavy
        // state files so concurrent readers take a shared lock from the
        // first access rather than the lock-free path (see `FileLock::shared`).
        for data in [
            self.config_path(),
            self.image_registry_path(),
            self.network_allocations_path(),
        ] {
            let lock = lock_path_for(&data);
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock)
                .map_err(|e| Error::Io {
                    path: lock,
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

    /// Path to the global config file.
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Directory for kernel binaries.
    pub fn kernel_dir(&self) -> PathBuf {
        self.root.join("kernels")
    }

    /// Acquire the exclusive per-VM operation lock for `name`.
    ///
    /// Serializes whole lifecycle commands (create / start / stop / delete /
    /// rename / …) for a single VM: while one process holds it, another that
    /// targets the same name blocks until the first finishes. This prevents
    /// interleavings the per-file content locks can't — double-start, start
    /// racing delete, or two creates of the same name. Different names lock
    /// independently.
    ///
    /// The lock is released when the returned guard is dropped. This is
    /// distinct from the content locks taken inside [`update`](Self::update) /
    /// [`create`](Self::create); a command holds the op lock for its whole
    /// duration and takes content locks briefly within. Do not acquire it
    /// twice for the same name in one process — `flock` would self-block.
    ///
    /// The lock file lives in a dedicated `locks/` directory, not inside the
    /// per-VM directory, so deleting or renaming a VM never removes a lock
    /// that another process is blocked on (which would defeat the exclusion).
    /// Lock files are intentionally never removed.
    pub fn lock_vm(&self, name: &str) -> Result<VmOpLock> {
        let path = self.root.join("locks").join(format!("{name}.lock"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;
        let flock = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| Error::Io {
            path,
            source: errno.into(),
        })?;
        Ok(VmOpLock { _flock: flock })
    }

    /// Read and deserialize a JSON file, using a shared (read) lock.
    ///
    /// Returns an error if the file does not exist or cannot be parsed.
    pub fn read<T: DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let _lock = FileLock::shared(path)?;
        self.read_unlocked(path)?.ok_or_else(|| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        })
    }

    /// Read and deserialize a JSON file, returning `None` if it doesn't exist.
    ///
    /// Uses a shared (read) lock. Returns an error only on I/O failures
    /// other than "not found" or on parse errors.
    pub fn read_optional<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        let _lock = FileLock::shared(path)?;
        self.read_unlocked(path)
    }

    /// Atomically read-modify-write a JSON file under a single exclusive lock.
    ///
    /// The lock is held across the read, the closure, and the write, so two
    /// concurrent callers serialize and neither loses the other's change.
    /// Errors if the file does not exist — use [`update_with`](Self::update_with)
    /// for state that materializes on first write.
    ///
    /// The closure must be cheap: it runs while the exclusive lock is held,
    /// blocking every other reader and writer of this file. Do not perform
    /// I/O, downloads, or process spawns inside it — do that work first and
    /// apply only the resulting field changes here.
    pub fn update<T, R>(&self, path: &Path, f: impl FnOnce(&mut T) -> Result<R>) -> Result<R>
    where
        T: Serialize + DeserializeOwned,
    {
        let _lock = FileLock::exclusive(path)?;
        let mut value: T = self
            .read_unlocked(path)?
            .ok_or_else(|| Error::State(format!("{}: not found", path.display())))?;
        let result = f(&mut value)?;
        self.write_locked(path, &value)?;
        Ok(result)
    }

    /// Like [`update`](Self::update), but seeds the value from `default()`
    /// when the file does not exist yet.
    ///
    /// For shared accumulators (image registry, IP allocations) that are
    /// created lazily on their first mutation. The same cheap-closure
    /// contract as [`update`](Self::update) applies.
    pub fn update_with<T, R>(
        &self,
        path: &Path,
        default: impl FnOnce() -> T,
        f: impl FnOnce(&mut T) -> Result<R>,
    ) -> Result<R>
    where
        T: Serialize + DeserializeOwned,
    {
        let _lock = FileLock::exclusive(path)?;
        let mut value: T = self.read_unlocked(path)?.unwrap_or_else(default);
        let result = f(&mut value)?;
        self.write_locked(path, &value)?;
        Ok(result)
    }

    /// Atomically create a JSON file, failing if it already exists.
    ///
    /// The existence check and the write happen under one exclusive lock, so
    /// two concurrent creators cannot both succeed — exactly one wins and the
    /// other gets [`Error::AlreadyExists`]. Use for write-once state (the
    /// global config, a VM's initial metadata).
    pub fn create<T: Serialize>(&self, path: &Path, data: &T) -> Result<()> {
        let _lock = FileLock::exclusive(path)?;
        if path.exists() {
            return Err(Error::AlreadyExists {
                path: path.to_path_buf(),
            });
        }
        self.write_locked(path, data)
    }

    /// Serialize and write a JSON file atomically, taking the exclusive lock.
    ///
    /// Fire-and-forget overwrite, kept while callers migrate to
    /// [`update`](Self::update) / [`create`](Self::create); it does not guard
    /// against a lost update across a preceding read.
    pub fn write<T: Serialize>(&self, path: &Path, data: &T) -> Result<()> {
        let _lock = FileLock::exclusive(path)?;
        self.write_locked(path, data)
    }

    /// Deserialize the JSON file at `path`, returning `None` if it is absent.
    ///
    /// Takes no lock — the caller must already hold one.
    fn read_unlocked<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Serialize `data` to `path` via temp file + atomic `rename`, creating
    /// parent directories as needed.
    ///
    /// Takes no lock — the caller must already hold the exclusive lock.
    fn write_locked<T: Serialize>(&self, path: &Path, data: &T) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Write to a temp file in the same directory (same filesystem for rename).
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

/// RAII guard for the per-VM operation lock returned by
/// [`StateStore::lock_vm`]. Releases the lock when dropped.
pub struct VmOpLock {
    _flock: Flock<File>,
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
    use std::sync::Arc;

    #[test]
    fn create_rejects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        let path = dir.path().join("once.json");

        store.create(&path, &1u32).unwrap();
        let err = store.create(&path, &2u32).unwrap_err();
        assert!(matches!(err, Error::AlreadyExists { .. }));

        // The original value is untouched.
        let loaded: u32 = store.read(&path).unwrap();
        assert_eq!(loaded, 1);
    }

    #[test]
    fn update_errors_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        let path = dir.path().join("missing.json");

        let err = store.update(&path, |_: &mut u32| Ok(())).unwrap_err();
        assert!(matches!(err, Error::State(_)));
    }

    #[test]
    fn update_with_seeds_default_then_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        let path = dir.path().join("acc.json");

        // First call materializes from the default.
        store
            .update_with(
                &path,
                || vec![1u32],
                |v| {
                    v.push(2);
                    Ok(())
                },
            )
            .unwrap();
        // Second call reads back and appends.
        store
            .update_with(&path, Vec::new, |v| {
                v.push(3);
                Ok(())
            })
            .unwrap();

        let loaded: Vec<u32> = store.read(&path).unwrap();
        assert_eq!(loaded, vec![1, 2, 3]);
    }

    #[test]
    fn concurrent_updates_do_not_lose_increments() {
        // The core lost-update regression test: N threads each do M locked
        // read-modify-write increments against the same file. Without the
        // lock spanning the whole transaction, increments would be lost.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().to_path_buf()));
        store.init().unwrap();
        let path = Arc::new(dir.path().join("counter.json"));

        store.create(&path, &0u64).unwrap();

        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 50;

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let store = Arc::clone(&store);
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        store
                            .update(&path, |n: &mut u64| {
                                *n += 1;
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let final_count: u64 = store.read(&path).unwrap();
        assert_eq!(final_count, THREADS * PER_THREAD);
    }

    #[test]
    fn concurrent_creates_have_exactly_one_winner() {
        // Many threads race to create the same file; exactly one succeeds.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().to_path_buf()));
        store.init().unwrap();
        let path = Arc::new(dir.path().join("unique.json"));

        const THREADS: usize = 16;
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let store = Arc::clone(&store);
                let path = Arc::clone(&path);
                std::thread::spawn(move || store.create(&path, &(i as u32)).is_ok())
            })
            .collect();

        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&ok| ok)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn lock_vm_serializes_same_name() {
        // Two threads taking the op-lock for the same VM name must not hold it
        // simultaneously. We assert mutual exclusion by counting overlaps.
        use std::sync::atomic::{AtomicI32, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().to_path_buf()));
        store.init().unwrap();

        let active = Arc::new(AtomicI32::new(0));
        let max_seen = Arc::new(AtomicI32::new(0));

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let store = Arc::clone(&store);
                let active = Arc::clone(&active);
                let max_seen = Arc::clone(&max_seen);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _guard = store.lock_vm("shared").unwrap();
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        // Tiny critical section; any overlap would push `now` past 1.
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lock_vm_distinct_names_are_independent() {
        // Different names must be lockable at the same time (no global lock).
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let a = store.lock_vm("alpha").unwrap();
        let b = store.lock_vm("beta").unwrap();
        drop((a, b));
    }

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
        store.create(&path, &data).unwrap();

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
        store.create(&path, &data).unwrap();

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
        store.create(&path, &"hello").unwrap();
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
        store.create(&path, &42u32).unwrap();

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
            store.config_path(),
            PathBuf::from("/var/lib/ember/config.json")
        );
        assert_eq!(store.kernel_dir(), PathBuf::from("/var/lib/ember/kernels"));
    }
}
