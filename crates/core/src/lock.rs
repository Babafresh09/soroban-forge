//! Advisory locking for the on-disk stores.
//!
//! [`crate::atomic::write_atomic`] makes a single store write safe against a
//! crash, but it does not make a *sequence* of operations safe against another
//! process. Every store mutation is a read-modify-write:
//!
//! ```text
//! load_store(path)  ->  mutate the in-memory store  ->  save_store(path, …)
//! ```
//!
//! Two concurrent `soroban-forge identity generate` invocations (a script
//! generating several identities in parallel, say) can both load the same
//! store, each insert their own identity into their own copy, and then write in
//! turn — the second write wins and the first identity is gone, even though its
//! command printed a public key and exited `0`. Atomic writes do not help here:
//! both writes are individually complete, they just overwrite each other.
//!
//! The fix is to serialize the whole sequence with an advisory lock taken on a
//! sibling lock file, so the second writer loads the store only after the first
//! has saved it. "Advisory" because nothing stops a process that does not take
//! the lock from writing the store anyway — every writer inside soroban-forge
//! goes through [`with_lock`], which is what makes it effective.
//!
//! The lock is a lock *file* created with `create_new`, not a byte-range lock
//! on the store itself. Creating a file exclusively is a single atomic
//! filesystem operation on every platform soroban-forge supports, needs no
//! platform-specific code or extra dependency, and — unlike locking the store
//! file — keeps working across the `rename` that
//! [`crate::atomic::write_atomic`] performs, which replaces the very inode a
//! byte-range lock would be held on.
//!
//! A process that dies while holding the lock leaves the file behind, so a lock
//! file older than [`STALE_AFTER`] is treated as abandoned and broken. Without
//! that, one crash would wedge every later `identity` and `network` command.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::{ForgeError, Result};

/// How long to wait for another writer to release the lock before giving up.
///
/// Generous enough to cover a slow disk or a burst of parallel invocations,
/// short enough that a wedged command still fails inside a CI step's patience.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a lock file may sit untouched before it is treated as abandoned by
/// a crashed process and broken. Must comfortably exceed [`ACQUIRE_TIMEOUT`],
/// or a writer that is merely waiting could have its own lock stolen.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// How often to retry while the lock is held by someone else.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A held lock on a store file. Releasing happens on drop, including while
/// unwinding from a panic or an early `?` return inside [`with_lock`].
#[derive(Debug)]
pub struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // A failure here only means the next writer waits out the stale
        // timeout instead of acquiring immediately; there is nothing useful to
        // report from a destructor.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The lock file guarding `store`: a sibling named `<store>.lock`.
pub fn lock_path(store: &Path) -> PathBuf {
    let mut name = store.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    store.with_file_name(name)
}

/// Run `f` with the lock on `store` held, releasing it afterwards.
///
/// The whole load-modify-save sequence belongs inside `f`: locking only the
/// save would still let a second writer load a store that is about to go
/// stale.
///
/// Not reentrant — `f` must not itself call `with_lock` on the same store.
pub fn with_lock<T>(store: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = acquire(store)?;
    f()
}

/// Acquire the lock on `store`, waiting up to [`ACQUIRE_TIMEOUT`] for another
/// writer to finish.
pub fn acquire(store: &Path) -> Result<StoreLock> {
    let path = lock_path(store);

    // The store's directory may not exist yet: the first `identity generate`
    // on a new machine creates `~/.config/soroban-forge` along with the store.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ForgeError::io(format!("creating {}", parent.display())))?;
    }

    let deadline = Instant::now() + ACQUIRE_TIMEOUT;
    loop {
        match try_create(&path) {
            Ok(()) => return Ok(StoreLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if break_if_stale(&path) {
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(ForgeError::Other(format!(
                        "timed out after {}s waiting for another soroban-forge process to finish writing {}\nhint: if no other soroban-forge is running, delete {}",
                        ACQUIRE_TIMEOUT.as_secs(),
                        store.display(),
                        path.display()
                    )));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                return Err(ForgeError::io(format!(
                    "creating lock file {}",
                    path.display()
                ))(e))
            }
        }
    }
}

/// Create the lock file, failing with `AlreadyExists` if another writer holds
/// it. `create_new` is the atomic test-and-set this whole module rests on.
fn try_create(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    // Recorded only so a human looking at a leftover lock file can tell which
    // process to go looking for; nothing reads it back.
    let _ = writeln!(file, "{}", std::process::id());
    Ok(())
}

/// Remove `path` if it was left behind by a process that died holding it.
/// Returns whether anything was removed, i.e. whether a retry is worthwhile.
fn break_if_stale(path: &Path) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        // Either the holder released it between our create and this check — in
        // which case retrying is right anyway — or the file is unreadable, and
        // the next create will keep reporting the real error.
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        // Lock file timestamped in the future (clock skew, a networked
        // filesystem): not evidence of abandonment, so leave it alone.
        return false;
    };
    if age < STALE_AFTER {
        return false;
    }
    log::warn!(
        "breaking stale lock {} (untouched for {}s)",
        path.display(),
        age.as_secs()
    );
    // The loser of a race between two breakers simply retries the create.
    std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("soroban-forge-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_lock_file_is_a_sibling_of_the_store() {
        let path = lock_path(Path::new("/cfg/soroban-forge/identities.json"));
        assert_eq!(
            path,
            PathBuf::from("/cfg/soroban-forge/identities.json.lock")
        );
    }

    #[test]
    fn the_lock_is_released_when_the_guard_drops() {
        let dir = temp_dir("release");
        let store = dir.join("store.json");

        drop(acquire(&store).unwrap());

        assert!(!lock_path(&store).exists(), "lock file outlived its guard");
        // And so the next writer can take it.
        drop(acquire(&store).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_acquire_waits_while_the_first_is_held() {
        let dir = temp_dir("contended");
        let store = dir.join("store.json");
        let guard = acquire(&store).unwrap();

        // The lock is held, so the second acquire must not succeed
        // immediately. Release it from another thread a fraction of the
        // acquire timeout later and confirm the waiter gets through rather
        // than erroring out.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
        });

        let started = Instant::now();
        let second = acquire(&store).unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "second acquire did not wait for the first to release"
        );
        drop(second);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_lock_releases_the_lock_when_the_body_fails() {
        let dir = temp_dir("body-error");
        let store = dir.join("store.json");

        let result: Result<()> = with_lock(&store, || Err(ForgeError::Other("boom".into())));

        assert!(result.is_err());
        assert!(
            !lock_path(&store).exists(),
            "a failed mutation must not leave the store locked"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_store_directory_is_created_if_missing() {
        let dir = temp_dir("mkdir");
        let store = dir.join("nested").join("identities.json");

        let guard = acquire(&store).unwrap();

        assert!(lock_path(&store).exists());
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_abandoned_lock_file_is_broken() {
        // A process that died holding the lock must not wedge every later
        // command. Backdate the lock file past the stale threshold to stand in
        // for the crash, then confirm the lock can be taken well inside the
        // acquire timeout.
        let dir = temp_dir("stale");
        let store = dir.join("store.json");
        let lock = lock_path(&store);
        std::fs::write(&lock, "99999\n").unwrap();
        backdate(&lock, STALE_AFTER + Duration::from_secs(5));

        let started = Instant::now();
        let guard = acquire(&store).unwrap();

        assert!(
            started.elapsed() < ACQUIRE_TIMEOUT,
            "a stale lock was waited out instead of broken"
        );
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_lock_file_is_not_broken() {
        let dir = temp_dir("not-stale");
        let store = dir.join("store.json");
        let lock = lock_path(&store);
        std::fs::write(&lock, "99999\n").unwrap();

        assert!(
            !break_if_stale(&lock),
            "a lock a live process could still be holding must not be broken"
        );
        assert!(lock.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Set `path`'s mtime `age` into the past, so staleness can be tested
    /// without sleeping for a minute.
    fn backdate(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }
}
