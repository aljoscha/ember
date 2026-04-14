//! Inject configuration files into an unpacked rootfs directory.
//!
//! Prepares a rootfs for VM use by injecting:
//! - SSH `authorized_keys` so the host can connect to the guest
//! - `/etc/resolv.conf` for DNS resolution
//!
//! Called after OCI layer extraction and before ext4 image creation.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::backend::ResolvConfMode;
use crate::error::{Error, Result};

/// Common SSH public key filenames, in preference order.
const SSH_PUBKEY_NAMES: &[&str] = &["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"];

/// Common SSH private key filenames, in preference order.
const SSH_PRIVKEY_NAMES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];

/// Return the default SSH public key path.
///
/// Looks in the invoking user's `~/.ssh/` directory for common key types.
/// When running under `sudo`, uses `SUDO_USER` to resolve the real user's
/// home directory instead of root's.
pub fn default_ssh_pubkey_path() -> Option<PathBuf> {
    let home = invoking_user_home()?;
    find_ssh_pubkey_in(&home.join(".ssh"))
}

/// Return the default SSH private key path.
///
/// Looks in the invoking user's `~/.ssh/` directory for common key types.
/// When running under `sudo`, uses `SUDO_USER` to resolve the real user's
/// home directory instead of root's.
pub fn default_ssh_privkey_path() -> Option<PathBuf> {
    let home = invoking_user_home()?;
    let ssh_dir = home.join(".ssh");
    SSH_PRIVKEY_NAMES
        .iter()
        .map(|name| ssh_dir.join(name))
        .find(|p| p.exists())
}

/// Find the first matching SSH public key in the given `.ssh` directory.
fn find_ssh_pubkey_in(ssh_dir: &Path) -> Option<PathBuf> {
    SSH_PUBKEY_NAMES
        .iter()
        .map(|name| ssh_dir.join(name))
        .find(|p| p.exists())
}

/// Inject an SSH `authorized_keys` file into the rootfs for the root user.
///
/// Convenience wrapper around [`inject_ssh_authorized_keys_for_home`] that
/// targets `/root/.ssh/authorized_keys`.
pub fn inject_ssh_authorized_keys(rootfs_dir: &Path, pubkey_path: &Path) -> Result<()> {
    inject_ssh_authorized_keys_for_home(rootfs_dir, pubkey_path, "root")
}

/// Inject an SSH `authorized_keys` file into a user's home directory in the rootfs.
///
/// `home_relative` is the path relative to the rootfs root — e.g. `"root"` for
/// the root user or `"home/ubuntu"` for the ubuntu user. Permissions are set to
/// 700 for `.ssh/` and 600 for `authorized_keys` as required by OpenSSH.
///
/// For non-root users, the `.ssh/` directory and `authorized_keys` file are
/// chowned to match the ownership of the home directory. This is required
/// because OpenSSH's `StrictModes` rejects keys not owned by the target user.
pub fn inject_ssh_authorized_keys_for_home(
    rootfs_dir: &Path,
    pubkey_path: &Path,
    home_relative: &str,
) -> Result<()> {
    let pubkey = fs::read_to_string(pubkey_path).map_err(|e| Error::Io {
        path: pubkey_path.to_path_buf(),
        source: e,
    })?;

    if pubkey.trim().is_empty() {
        return Err(Error::Image(format!(
            "SSH public key file is empty: {}",
            pubkey_path.display()
        )));
    }

    let home_dir = rootfs_dir.join(home_relative);
    let ssh_dir = home_dir.join(".ssh");
    fs::create_dir_all(&ssh_dir).map_err(|e| Error::Io {
        path: ssh_dir.clone(),
        source: e,
    })?;
    fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700)).map_err(|e| Error::Io {
        path: ssh_dir.clone(),
        source: e,
    })?;

    let authorized_keys_path = ssh_dir.join("authorized_keys");
    fs::write(&authorized_keys_path, pubkey.as_bytes()).map_err(|e| Error::Io {
        path: authorized_keys_path.clone(),
        source: e,
    })?;
    fs::set_permissions(&authorized_keys_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
        Error::Io {
            path: authorized_keys_path.clone(),
            source: e,
        }
    })?;

    // For non-root users, chown .ssh/ and authorized_keys to match the home
    // directory's owner. OpenSSH StrictModes requires this.
    if home_relative != "root" {
        let meta = fs::metadata(&home_dir).map_err(|e| Error::Io {
            path: home_dir.clone(),
            source: e,
        })?;
        let uid = meta.uid();
        let gid = meta.gid();

        chown_path(&ssh_dir, uid, gid)?;
        chown_path(&authorized_keys_path, uid, gid)?;
    }

    Ok(())
}

/// Detect the preferred SSH user for a rootfs.
///
/// Checks whether `/home/ubuntu` exists in the rootfs. If so, returns
/// `("ubuntu", "home/ubuntu")`. Otherwise falls back to `("root", "root")`.
///
/// This heuristic works because our ubuntu-vm Dockerfile creates
/// `/home/ubuntu`, while pulled Alpine/other images only have `/root`.
pub fn detect_ssh_user(rootfs_dir: &Path) -> (&'static str, &'static str) {
    if rootfs_dir.join("home/ubuntu").is_dir() {
        ("ubuntu", "home/ubuntu")
    } else {
        ("root", "root")
    }
}

/// Set ownership of a path (file or directory).
fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from_raw_os_error(e as i32),
    })
}

/// Inject `/etc/hosts` into the rootfs for localhost resolution.
///
/// Writes a minimal `/etc/hosts` with the standard loopback entries.
/// Without this, `localhost` is unresolvable in many container-derived
/// images (which ship without `/etc/hosts`), causing tools like `psql`
/// to fail when connecting to `localhost` instead of `127.0.0.1`.
///
/// Any existing `/etc/hosts` is replaced — container images often have
/// stale or Docker-specific entries that don't apply inside a VM.
pub fn inject_hosts(rootfs_dir: &Path) -> Result<()> {
    write_hosts(rootfs_dir, None)
}

/// Inject `/etc/hosts` with the VM's hostname for correct name resolution.
///
/// Like [`inject_hosts`], but also adds the VM hostname to the loopback
/// entries. Without this, `sudo` (and anything else that resolves the
/// machine's own hostname) prints "unable to resolve host" warnings.
///
/// Called at VM creation time when the VM name is known.
pub fn inject_hosts_with_hostname(rootfs_dir: &Path, hostname: &str) -> Result<()> {
    write_hosts(rootfs_dir, Some(hostname))
}

/// Write `/etc/hosts` with loopback entries and an optional hostname.
fn write_hosts(rootfs_dir: &Path, hostname: Option<&str>) -> Result<()> {
    let etc_dir = rootfs_dir.join("etc");
    fs::create_dir_all(&etc_dir).map_err(|e| Error::Io {
        path: etc_dir.clone(),
        source: e,
    })?;

    let hosts_path = etc_dir.join("hosts");

    if hosts_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&hosts_path);
    }

    let contents = match hostname {
        Some(name) => format!(
            "127.0.0.1\tlocalhost {name}\n::1\t\tlocalhost ip6-localhost ip6-loopback {name}\n"
        ),
        None => "127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n".to_string(),
    };

    fs::write(&hosts_path, contents).map_err(|e| Error::Io {
        path: hosts_path,
        source: e,
    })?;

    Ok(())
}

/// Inject `/etc/resolv.conf` into the rootfs for DNS resolution.
///
/// **Linux**: Creates a symlink `/etc/resolv.conf` → `/proc/net/pnp`. The
/// kernel populates `/proc/net/pnp` with DNS servers from the `ip=` boot
/// parameter, so DNS is configured dynamically at every VM boot without
/// baking server addresses into the image.
///
/// **macOS**: Writes a static `/etc/resolv.conf` with public DNS servers.
/// vmnet shared mode's DHCP advertises the gateway (192.168.64.1) as DNS,
/// but the gateway doesn't actually forward DNS queries. The kernel's
/// `ip=dhcp` picks up this non-functional server in `/proc/net/pnp`, so
/// we use a static file instead.
///
/// If the rootfs already has a `resolv.conf` (possibly a symlink from the
/// container image, e.g. Ubuntu's link to `/run/systemd/resolve/...`),
/// it is removed first.
pub fn inject_resolv_conf(rootfs_dir: &Path, mode: &ResolvConfMode) -> Result<()> {
    let etc_dir = rootfs_dir.join("etc");
    fs::create_dir_all(&etc_dir).map_err(|e| Error::Io {
        path: etc_dir.clone(),
        source: e,
    })?;

    let resolv_path = etc_dir.join("resolv.conf");

    // Remove existing resolv.conf — may be a symlink in some container images.
    if resolv_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&resolv_path);
    }

    match mode {
        ResolvConfMode::StaticContent(content) => {
            fs::write(&resolv_path, content).map_err(|e| Error::Io {
                path: resolv_path,
                source: e,
            })?;
        }
        ResolvConfMode::Symlink(target) => {
            std::os::unix::fs::symlink(target, &resolv_path).map_err(|e| Error::Io {
                path: resolv_path,
                source: e,
            })?;
        }
    }

    Ok(())
}

/// Inject `/etc/inittab` into the rootfs for proper VM init behaviour.
///
/// OCI container images aren't designed to boot as VMs — they typically lack
/// an inittab that handles Ctrl+Alt+Del. Without a `ctrlaltdel` entry,
/// Firecracker's `SendCtrlAltDel` action is ignored by the guest and the VM
/// has to be killed with SIGKILL instead of shutting down gracefully.
///
/// This writes a minimal busybox-init-compatible inittab that:
/// - Runs `/sbin/init` startup scripts (OpenRC `sysinit`/`boot`/`default` if present)
/// - Spawns a login shell on the serial console (`ttyS0`)
/// - Maps Ctrl+Alt+Del to `/sbin/reboot`
/// - Runs shutdown scripts on halt/reboot
///
/// Any existing inittab is replaced.
pub fn inject_inittab(rootfs_dir: &Path, console_device: &str) -> Result<()> {
    let etc_dir = rootfs_dir.join("etc");
    fs::create_dir_all(&etc_dir).map_err(|e| Error::Io {
        path: etc_dir.clone(),
        source: e,
    })?;

    let inittab_path = etc_dir.join("inittab");

    // Remove existing inittab (may be a symlink in some images).
    if inittab_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&inittab_path);
    }

    let console = console_device;

    let contents = format!(
        "\
# Generated by ember — minimal inittab for VM boot.
::sysinit:/sbin/openrc sysinit 2>/dev/null
::sysinit:/sbin/openrc boot 2>/dev/null
::wait:/sbin/openrc default 2>/dev/null
{console}::respawn:/sbin/getty 115200 {console}
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown 2>/dev/null
"
    );

    fs::write(&inittab_path, contents).map_err(|e| Error::Io {
        path: inittab_path,
        source: e,
    })?;

    Ok(())
}

/// Resolve the invoking (non-root) user's home directory.
///
/// Under `sudo`, `HOME` is typically `/root`. We check `SUDO_USER` first
/// and look up that user's home via `getpwnam`. Falls back to `HOME`.
fn invoking_user_home() -> Option<PathBuf> {
    // Try SUDO_USER first (set by sudo to the original user).
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Some(home) = home_for_user(&sudo_user) {
            return Some(home);
        }
    }

    // Fall back to HOME.
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Look up a user's home directory via getpwnam.
fn home_for_user(username: &str) -> Option<PathBuf> {
    use std::ffi::CString;
    let name = CString::new(username).ok()?;
    // SAFETY: getpwnam is passed a valid C string. The returned pointer
    // is to a static buffer — we copy the home_dir field immediately.
    let pw = unsafe { nix::libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let home_dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
    Some(PathBuf::from(home_dir.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_ssh_creates_authorized_keys() {
        let rootfs = tempfile::tempdir().unwrap();
        let keyfile = rootfs.path().join("test_key.pub");
        fs::write(&keyfile, "ssh-ed25519 AAAA... user@host\n").unwrap();

        inject_ssh_authorized_keys(rootfs.path(), &keyfile).unwrap();

        let ak = rootfs.path().join("root/.ssh/authorized_keys");
        assert!(ak.exists());
        assert_eq!(
            fs::read_to_string(&ak).unwrap(),
            "ssh-ed25519 AAAA... user@host\n"
        );
    }

    #[test]
    fn inject_ssh_sets_permissions() {
        let rootfs = tempfile::tempdir().unwrap();
        let keyfile = rootfs.path().join("test_key.pub");
        fs::write(&keyfile, "ssh-ed25519 AAAA... user@host\n").unwrap();

        inject_ssh_authorized_keys(rootfs.path(), &keyfile).unwrap();

        let ssh_dir = rootfs.path().join("root/.ssh");
        let dir_mode = fs::metadata(&ssh_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "~/.ssh should be 700");

        let ak = ssh_dir.join("authorized_keys");
        let file_mode = fs::metadata(&ak).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "authorized_keys should be 600");
    }

    #[test]
    fn inject_ssh_empty_key_fails() {
        let rootfs = tempfile::tempdir().unwrap();
        let keyfile = rootfs.path().join("empty.pub");
        fs::write(&keyfile, "  \n").unwrap();

        let result = inject_ssh_authorized_keys(rootfs.path(), &keyfile);
        assert!(result.is_err());
    }

    #[test]
    fn inject_ssh_missing_key_fails() {
        let rootfs = tempfile::tempdir().unwrap();
        let result = inject_ssh_authorized_keys(rootfs.path(), Path::new("/nonexistent/key.pub"));
        assert!(result.is_err());
    }

    #[test]
    fn inject_ssh_existing_ssh_dir_is_ok() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("root/.ssh")).unwrap();

        let keyfile = rootfs.path().join("test_key.pub");
        fs::write(&keyfile, "ssh-ed25519 AAAA... user@host\n").unwrap();

        inject_ssh_authorized_keys(rootfs.path(), &keyfile).unwrap();

        let ak = rootfs.path().join("root/.ssh/authorized_keys");
        assert!(ak.exists());
    }

    #[test]
    fn inject_ssh_for_home_creates_authorized_keys() {
        let rootfs = tempfile::tempdir().unwrap();
        // Create the home directory to simulate useradd.
        fs::create_dir_all(rootfs.path().join("home/ubuntu")).unwrap();

        let keyfile = rootfs.path().join("test_key.pub");
        fs::write(&keyfile, "ssh-ed25519 AAAA... user@host\n").unwrap();

        inject_ssh_authorized_keys_for_home(rootfs.path(), &keyfile, "home/ubuntu").unwrap();

        let ak = rootfs.path().join("home/ubuntu/.ssh/authorized_keys");
        assert!(ak.exists());
        assert_eq!(
            fs::read_to_string(&ak).unwrap(),
            "ssh-ed25519 AAAA... user@host\n"
        );
    }

    #[test]
    fn inject_ssh_for_home_sets_permissions() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("home/ubuntu")).unwrap();

        let keyfile = rootfs.path().join("test_key.pub");
        fs::write(&keyfile, "ssh-ed25519 AAAA... user@host\n").unwrap();

        inject_ssh_authorized_keys_for_home(rootfs.path(), &keyfile, "home/ubuntu").unwrap();

        let ssh_dir = rootfs.path().join("home/ubuntu/.ssh");
        let dir_mode = fs::metadata(&ssh_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "~/.ssh should be 700");

        let ak = ssh_dir.join("authorized_keys");
        let file_mode = fs::metadata(&ak).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "authorized_keys should be 600");
    }

    #[test]
    fn detect_ssh_user_with_ubuntu() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("home/ubuntu")).unwrap();

        let (user, home) = detect_ssh_user(rootfs.path());
        assert_eq!(user, "ubuntu");
        assert_eq!(home, "home/ubuntu");
    }

    #[test]
    fn detect_ssh_user_without_ubuntu() {
        let rootfs = tempfile::tempdir().unwrap();
        // No /home/ubuntu — only root.
        fs::create_dir_all(rootfs.path().join("root")).unwrap();

        let (user, home) = detect_ssh_user(rootfs.path());
        assert_eq!(user, "root");
        assert_eq!(home, "root");
    }

    #[test]
    fn inject_hosts_creates_file() {
        let rootfs = tempfile::tempdir().unwrap();
        inject_hosts(rootfs.path()).unwrap();

        let hosts = rootfs.path().join("etc/hosts");
        let contents = fs::read_to_string(&hosts).unwrap();
        assert!(contents.contains("127.0.0.1\tlocalhost"));
        assert!(contents.contains("::1"));
    }

    #[test]
    fn inject_hosts_creates_etc_dir() {
        let rootfs = tempfile::tempdir().unwrap();
        inject_hosts(rootfs.path()).unwrap();

        let hosts = rootfs.path().join("etc/hosts");
        assert!(hosts.exists());
    }

    #[test]
    fn inject_hosts_replaces_existing() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("hosts"), "old content").unwrap();

        inject_hosts(rootfs.path()).unwrap();

        let contents = fs::read_to_string(etc.join("hosts")).unwrap();
        assert!(contents.contains("127.0.0.1"));
        assert!(!contents.contains("old content"));
    }

    #[test]
    fn inject_hosts_with_hostname_includes_vm_name() {
        let rootfs = tempfile::tempdir().unwrap();
        inject_hosts_with_hostname(rootfs.path(), "my-test-vm").unwrap();

        let hosts = rootfs.path().join("etc/hosts");
        let contents = fs::read_to_string(&hosts).unwrap();
        assert!(contents.contains("127.0.0.1\tlocalhost my-test-vm"));
        assert!(contents.contains("::1\t\tlocalhost ip6-localhost ip6-loopback my-test-vm"));
    }

    #[test]
    fn inject_hosts_without_hostname_excludes_vm_name() {
        let rootfs = tempfile::tempdir().unwrap();
        inject_hosts(rootfs.path()).unwrap();

        let hosts = rootfs.path().join("etc/hosts");
        let contents = fs::read_to_string(&hosts).unwrap();
        assert_eq!(
            contents,
            "127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n"
        );
    }

    #[test]
    fn inject_resolv_conf_symlink_mode() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();

        let mode = ResolvConfMode::Symlink("/proc/net/pnp");
        inject_resolv_conf(rootfs.path(), &mode).unwrap();

        let resolv = rootfs.path().join("etc/resolv.conf");
        let meta = resolv.symlink_metadata().unwrap();
        assert!(meta.is_symlink(), "resolv.conf should be a symlink");
        let target = fs::read_link(&resolv).unwrap();
        assert_eq!(
            target.to_str().unwrap(),
            "/proc/net/pnp",
            "resolv.conf should point to /proc/net/pnp"
        );
    }

    #[test]
    fn inject_resolv_conf_static_mode() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();

        let mode = ResolvConfMode::StaticContent("nameserver 8.8.8.8\nnameserver 1.1.1.1\n");
        inject_resolv_conf(rootfs.path(), &mode).unwrap();

        let resolv = rootfs.path().join("etc/resolv.conf");
        let contents = fs::read_to_string(&resolv).unwrap();
        assert!(
            contents.contains("nameserver 8.8.8.8"),
            "resolv.conf should contain public DNS"
        );
    }

    #[test]
    fn inject_resolv_conf_creates_etc_dir() {
        let rootfs = tempfile::tempdir().unwrap();
        // No etc/ dir yet — inject should create it.
        let mode = ResolvConfMode::Symlink("/proc/net/pnp");
        inject_resolv_conf(rootfs.path(), &mode).unwrap();

        let resolv = rootfs.path().join("etc/resolv.conf");
        assert!(resolv.symlink_metadata().is_ok());
    }

    #[test]
    fn inject_resolv_conf_replaces_existing() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("resolv.conf"), "old content").unwrap();

        let mode = ResolvConfMode::Symlink("/proc/net/pnp");
        inject_resolv_conf(rootfs.path(), &mode).unwrap();

        let resolv = etc.join("resolv.conf");
        let contents = fs::read_to_string(&resolv).unwrap_or_default();
        assert!(!contents.contains("old content"));
    }

    #[test]
    fn inject_resolv_conf_replaces_symlink() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();

        // Simulate Ubuntu's symlink: /etc/resolv.conf → /run/systemd/resolve/resolv.conf
        std::os::unix::fs::symlink("/run/systemd/resolve/resolv.conf", etc.join("resolv.conf"))
            .unwrap();

        let mode = ResolvConfMode::Symlink("/proc/net/pnp");
        inject_resolv_conf(rootfs.path(), &mode).unwrap();

        let resolv = etc.join("resolv.conf");
        // Old symlink should be gone.
        let target = fs::read_link(&resolv).unwrap();
        assert_eq!(
            target.to_str().unwrap(),
            "/proc/net/pnp",
            "should replace old symlink with /proc/net/pnp"
        );
    }

    #[test]
    fn inject_inittab_creates_file_ttys0() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();

        inject_inittab(rootfs.path(), "ttyS0").unwrap();

        let inittab = rootfs.path().join("etc/inittab");
        assert!(inittab.exists());

        let contents = fs::read_to_string(&inittab).unwrap();
        assert!(contents.contains("::ctrlaltdel:/sbin/reboot"));
        assert!(contents.contains("ttyS0::respawn"));
    }

    #[test]
    fn inject_inittab_creates_file_hvc0() {
        let rootfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();

        inject_inittab(rootfs.path(), "hvc0").unwrap();

        let inittab = rootfs.path().join("etc/inittab");
        assert!(inittab.exists());

        let contents = fs::read_to_string(&inittab).unwrap();
        assert!(contents.contains("::ctrlaltdel:/sbin/reboot"));
        assert!(contents.contains("hvc0::respawn"));
    }

    #[test]
    fn inject_inittab_creates_etc_dir() {
        let rootfs = tempfile::tempdir().unwrap();

        inject_inittab(rootfs.path(), "ttyS0").unwrap();

        let inittab = rootfs.path().join("etc/inittab");
        assert!(inittab.exists());
    }

    #[test]
    fn inject_inittab_replaces_existing() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("inittab"), "old content").unwrap();

        inject_inittab(rootfs.path(), "ttyS0").unwrap();

        let contents = fs::read_to_string(etc.join("inittab")).unwrap();
        assert!(contents.contains("::ctrlaltdel:/sbin/reboot"));
        assert!(!contents.contains("old content"));
    }

    #[test]
    fn invoking_user_home_returns_some() {
        // Should succeed unless HOME is unset.
        if std::env::var_os("HOME").is_some() {
            let home = invoking_user_home().unwrap();
            assert!(home.is_absolute());
        }
    }

    #[test]
    fn find_ssh_pubkey_prefers_ed25519() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path();
        fs::write(ssh_dir.join("id_ed25519.pub"), "ssh-ed25519 AAAA...\n").unwrap();
        fs::write(ssh_dir.join("id_rsa.pub"), "ssh-rsa AAAA...\n").unwrap();

        let path = find_ssh_pubkey_in(ssh_dir);
        assert!(path.is_some());
        assert!(path.unwrap().to_string_lossy().ends_with("id_ed25519.pub"));
    }

    #[test]
    fn find_ssh_pubkey_falls_back_to_rsa() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path();
        // Only RSA key exists.
        fs::write(ssh_dir.join("id_rsa.pub"), "ssh-rsa AAAA...\n").unwrap();

        let path = find_ssh_pubkey_in(ssh_dir);
        assert!(path.is_some());
        assert!(path.unwrap().to_string_lossy().ends_with("id_rsa.pub"));
    }

    #[test]
    fn find_ssh_pubkey_returns_none_when_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        // Empty directory, no key files.
        let path = find_ssh_pubkey_in(dir.path());
        assert!(path.is_none());
    }
}
