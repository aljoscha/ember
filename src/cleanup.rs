//! RAII guard for rolling back partially-completed multi-step operations.
//!
//! When an operation creates multiple resources in sequence (e.g., IP allocation,
//! TAP device, iptables rules, Firecracker process), a failure midway through
//! must clean up all previously-created resources. The [`Rollback`] guard
//! collects cleanup closures and executes them in reverse order on drop,
//! unless the operation succeeds and calls [`commit()`](Rollback::commit).

/// RAII guard that runs registered cleanup actions on drop unless committed.
///
/// Each successful resource creation pushes a cleanup closure. If the guard
/// is dropped without [`commit()`](Rollback::commit) (e.g., due to `?` early
/// return), all registered cleanups execute in LIFO order.
///
/// # Example
///
/// ```ignore
/// let mut rollback = Rollback::new();
///
/// create_tap_device()?;
/// rollback.push("TAP device", || delete_tap_device());
///
/// spawn_firecracker()?;
/// rollback.push("Firecracker process", || kill_firecracker());
///
/// // All steps succeeded — don't roll back.
/// rollback.commit();
/// ```
type CleanupAction = (&'static str, Box<dyn FnOnce()>);

pub(crate) struct Rollback {
    actions: Vec<CleanupAction>,
    committed: bool,
}

impl Rollback {
    pub fn new() -> Self {
        Self {
            actions: Vec::new(),
            committed: false,
        }
    }

    /// Register a cleanup action with a human-readable label.
    ///
    /// The label is printed during rollback so the user can see which
    /// resources are being cleaned up. Actions run in LIFO order.
    pub fn push(&mut self, label: &'static str, action: impl FnOnce() + 'static) {
        self.actions.push((label, Box::new(action)));
    }

    /// Mark the operation as successful — registered cleanups will NOT run.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Rollback {
    fn drop(&mut self) {
        if self.committed || self.actions.is_empty() {
            return;
        }
        eprintln!("Operation failed, rolling back...");
        for (label, action) in self.actions.drain(..).rev() {
            eprintln!("  Cleaning up {label}...");
            action();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn commit_prevents_rollback() {
        let log: Arc<Mutex<Vec<&str>>> = Arc::new(Mutex::new(Vec::new()));

        let mut rb = Rollback::new();
        let log2 = log.clone();
        rb.push("test", move || log2.lock().unwrap().push("rolled back"));
        rb.commit();

        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn drop_without_commit_runs_cleanups_in_lifo_order() {
        let log: Arc<Mutex<Vec<&str>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let mut rb = Rollback::new();
            let l = log.clone();
            rb.push("first", move || l.lock().unwrap().push("first"));
            let l = log.clone();
            rb.push("second", move || l.lock().unwrap().push("second"));
            let l = log.clone();
            rb.push("third", move || l.lock().unwrap().push("third"));
            // dropped without commit
        }

        let executed = log.lock().unwrap();
        assert_eq!(*executed, vec!["third", "second", "first"]);
    }

    #[test]
    fn empty_rollback_is_silent() {
        // Should not print "rolling back" when there are no actions.
        let _rb = Rollback::new();
    }
}
