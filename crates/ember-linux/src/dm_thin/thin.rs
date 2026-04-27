//! Thin volume operations.
//!
//! In dm-thin the same primitive serves three roles: a fresh thin volume
//! (no parent), a snapshot of an existing thin volume, and a clone for a
//! VM. Volumes are addressed by 64-bit numeric IDs allocated randomly by
//! [`allocate`] (see [`crate::dm_thin`] module docs and the spec).
//!
//! Volumes are not automatically activated as `/dev/mapper/...` devices —
//! callers must explicitly [`activate`] them when needed.

use std::path::PathBuf;
use std::process::Command;

use ember_core::error::{Error, Result};

use super::{is_already_exists, pool};

/// Device-mapper name prefix for image base volumes.
pub const IMAGE_PREFIX: &str = "ember-img-";
/// Device-mapper name prefix for VM disks.
pub const VM_PREFIX: &str = "ember-vm-";

/// Maximum thin device id accepted by the kernel.
///
/// `drivers/md/dm-thin.c` enforces `dev_id <= (1 << 24) - 1`:
///
/// ```text
/// if (*dev_id > MAX_DEV_ID) {
///     DMWARN("Message received with invalid device id: %llu", *dev_id);
///     return -EINVAL;
/// }
/// ```
///
/// Wider values were attempted earlier in this branch's history and
/// the kernel rejected them with `EINVAL`, so we generate ids inside
/// this 24-bit range.
pub const MAX_DEV_ID: u64 = (1 << 24) - 1;

/// Pick a fresh non-zero thin id within the kernel's 24-bit range.
///
/// Birthday collision at 50% hits around 4 K ids — well above any
/// realistic ember workload (hundreds of volumes per pool). The
/// kernel still rejects duplicates atomically and [`allocate`]
/// retries on `EEXIST`, so the rare collision is harmless.
fn fresh_thin_id() -> u64 {
    // Avoid id 0 — keeps logs/diagnostics easier to read.
    loop {
        let raw: u32 = rand::random();
        let id = (raw as u64) & MAX_DEV_ID;
        if id != 0 {
            return id;
        }
    }
}

/// Allocate a fresh thin volume in `pool` and return its id.
///
/// Picks a random `u64`, calls `create_thin`, and retries on the
/// vanishingly rare `EEXIST` collision.
pub fn allocate(pool_name: &str) -> Result<u64> {
    loop {
        let id = fresh_thin_id();
        match pool::message(pool_name, &format!("create_thin {id}")) {
            Ok(()) => return Ok(id),
            Err(e) if is_already_exists(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Allocate a fresh snapshot of `src_id` and return its new id.
///
/// Snapshots and thin volumes are the same primitive; the only
/// difference is the `create_snap` message specifies a parent.
pub fn allocate_snap(pool_name: &str, src_id: u64) -> Result<u64> {
    loop {
        let id = fresh_thin_id();
        match pool::message(pool_name, &format!("create_snap {id} {src_id}")) {
            Ok(()) => return Ok(id),
            Err(e) if is_already_exists(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Free a thin volume's id and release its blocks back to the pool.
///
/// The volume must not be activated as a device — call [`deactivate`]
/// first if necessary.
pub fn delete(pool_name: &str, thin_id: u64) -> Result<()> {
    pool::message(pool_name, &format!("delete {thin_id}"))
}

/// Path of a thin volume's device once activated.
pub fn device_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/dev/mapper/{name}"))
}

/// Whether a thin volume is currently activated as a `/dev/mapper`
/// device.
pub fn is_active(name: &str) -> Result<bool> {
    pool::exists(name)
}

/// Activate a thin volume as a `/dev/mapper/<name>` block device.
///
/// `size_sectors` is the volume's virtual size; the pool only allocates
/// blocks as the volume is written to.
pub fn activate(
    name: &str,
    pool_name: &str,
    thin_id: u64,
    size_sectors: u64,
) -> Result<PathBuf> {
    let table = thin_table(pool_name, thin_id, size_sectors);
    let output = Command::new("dmsetup")
        .args(["create", name, "--table", &table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup create".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup create thin", output)?;
    Ok(device_path(name))
}

/// Tear down a thin volume's `/dev/mapper` device. The underlying thin
/// id and its blocks remain in the pool until [`delete`] is called.
pub fn deactivate(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["remove", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup remove".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup remove", output)?;
    Ok(())
}

/// Suspend a thin volume's I/O. Required before snapshotting or
/// reloading the table.
pub fn suspend(name: &str) -> Result<()> {
    pool::suspend(name)
}

/// Resume a previously suspended thin volume.
pub fn resume(name: &str) -> Result<()> {
    pool::resume(name)
}

/// Reload the thin volume's table to expose a new virtual size.
///
/// Pool capacity is unaffected — thin volumes are virtually sized at
/// activation time and only consume blocks as they are written. Caller
/// is still responsible for filesystem-level resize (e.g. `resize2fs`).
pub fn reload_size(
    name: &str,
    pool_name: &str,
    thin_id: u64,
    new_size_sectors: u64,
) -> Result<()> {
    let table = thin_table(pool_name, thin_id, new_size_sectors);
    suspend(name)?;
    let load = Command::new("dmsetup")
        .args(["load", name, "--table", &table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup load".to_string(),
            source: e,
        })?;
    if let Err(e) = Error::check_command("dmsetup load thin", load) {
        let _ = resume(name);
        return Err(e);
    }
    resume(name)
}

fn thin_table(pool_name: &str, thin_id: u64, size_sectors: u64) -> String {
    let pool_dev = pool::device_path(pool_name);
    format!("0 {size_sectors} thin {} {thin_id}", pool_dev.display())
}

/// Sanitize an arbitrary name (image or VM) into a device-mapper-safe
/// component. dmsetup forbids `/`, `:`, and shell metacharacters; the
/// existing image/VM naming policy already enforces the right shape, so
/// this is a defensive guard rather than a real transformation.
pub fn sanitize_dm_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Device-mapper name for a VM volume.
pub fn vm_dm_name(vm_name: &str) -> String {
    format!("{VM_PREFIX}{}", sanitize_dm_name(vm_name))
}

/// Device-mapper name for an image base volume.
pub fn image_dm_name(image_name: &str) -> String {
    format!("{IMAGE_PREFIX}{}", sanitize_dm_name(image_name))
}

/// Device-mapper name for the temporary staging volume used while
/// writing a fresh image into the pool. Held only between
/// `create_thin` and the post-`dd` snapshot.
pub fn image_staging_dm_name(image_name: &str) -> String {
    format!("{IMAGE_PREFIX}{}-staging", sanitize_dm_name(image_name))
}

/// Path that should be passed to Firecracker as `path_on_host`.
pub fn vm_device_path(vm_name: &str) -> PathBuf {
    device_path(&vm_dm_name(vm_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_thin_id_is_nonzero_and_in_range() {
        for _ in 0..1000 {
            let id = fresh_thin_id();
            assert_ne!(id, 0);
            assert!(id <= MAX_DEV_ID, "id {id} exceeds kernel max {MAX_DEV_ID}");
        }
    }

    #[test]
    fn fresh_thin_id_distribution() {
        // 100 random ids in a 24-bit space collide with probability
        // ≈ 100²/(2·2²⁴) ≈ 3·10⁻⁴, so duplicates here would be a real bug.
        let ids: std::collections::HashSet<u64> =
            (0..100).map(|_| fresh_thin_id()).collect();
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn thin_table_shape() {
        let t = thin_table("ember-pool", 42, 16_777_216);
        assert_eq!(t, "0 16777216 thin /dev/mapper/ember-pool 42");
    }

    #[test]
    fn dm_names() {
        assert_eq!(vm_dm_name("myvm"), "ember-vm-myvm");
        assert_eq!(image_dm_name("library-alpine-latest"),
                   "ember-img-library-alpine-latest");
        assert_eq!(image_staging_dm_name("foo"), "ember-img-foo-staging");
    }

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize_dm_name("alpine_3.18-edge"), "alpine_3_18-edge");
        assert_eq!(sanitize_dm_name("my/vm:1"), "my_vm_1");
    }
}
