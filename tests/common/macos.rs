//! Shared macOS test helpers for integration tests.
//!
//! Provides ember-vz resolution, AVF kernel lookup, e2fsprogs/rootfs utilities,
//! APFS clone helpers, manual VM setup, and process management helpers.
//!
//! These are extracted from `common/mod.rs` (macOS-specific functions) and
//! `macos_storage.rs` (manual VM setup helpers) to consolidate all
//! macOS-specific helpers in one place. All functions are `pub` so test
//! files can use them via `common::macos::`.

use std::io::{BufRead, BufReader};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Ember init helper
// ---------------------------------------------------------------------------

/// Run `ember init` with a temporary state directory, returning the state dir path.
///
/// macOS-specific: no `--pool` or `--device` flags needed (APFS uses temp dirs).
/// Compare with `linux::setup_pool_and_init()` which creates a ZFS pool first.
pub fn setup_init(tmp: &Path) -> PathBuf {
    let state_dir = tmp.join("state");
    let output = super::ember(&["--state-dir", state_dir.to_str().unwrap(), "init"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    state_dir
}

// ---------------------------------------------------------------------------
// ember-vz helpers
// ---------------------------------------------------------------------------

/// Locate the `ember-vz` Swift helper binary.
///
/// Resolution order:
/// 1. `EMBER_VZ` env var
/// 2. `ember-vz/.build/release/ember-vz` (relative to project root)
/// 3. `ember-vz/.build/debug/ember-vz`
///
/// Panics if no binary is found.
pub fn ember_vz_bin() -> PathBuf {
    if let Ok(p) = std::env::var("EMBER_VZ") {
        let path = PathBuf::from(&p);
        assert!(path.exists(), "EMBER_VZ={p} does not exist");
        return path;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for subdir in ["release", "debug"] {
        let path = manifest_dir.join(format!("ember-vz/.build/{subdir}/ember-vz"));
        if path.exists() {
            return path;
        }
    }

    panic!(
        "ember-vz binary not found.\n\
         Build it with: cd ember-vz && swift build\n\
         Or set EMBER_VZ to the binary path."
    );
}

/// Get a bootable kernel for AVF tests.
///
/// Resolution order:
/// 1. `EMBER_TEST_KERNEL` env var (explicit override)
/// 2. Cached at `/tmp/ember-test-vmlinux`
/// 3. Local build at `kernel/vmlinux`
///
/// Panics if no kernel is available.
pub fn ensure_kernel() -> PathBuf {
    const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";

    if let Ok(p) = std::env::var("EMBER_TEST_KERNEL") {
        let path = PathBuf::from(&p);
        assert!(path.exists(), "EMBER_TEST_KERNEL={p} does not exist");
        return path;
    }

    let cache = PathBuf::from(KERNEL_CACHE_PATH);
    if cache.exists() {
        return cache;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let local_kernel = manifest_dir.join("kernel/vmlinux");
    if local_kernel.exists() {
        eprintln!("Using locally built kernel: {}", local_kernel.display());
        let _ = std::fs::copy(&local_kernel, &cache);
        return cache;
    }

    panic!(
        "No AVF-compatible kernel found.\n\
         Build one with: cd kernel && make docker-build ARCH=arm64 FRAGMENTS=\"avf.fragment\"\n\
         Or set EMBER_TEST_KERNEL to the kernel path."
    );
}

// ---------------------------------------------------------------------------
// e2fsprogs / rootfs helpers
// ---------------------------------------------------------------------------

/// Find an e2fsprogs tool by checking Homebrew paths before falling back to PATH.
///
/// macOS doesn't ship e2fsprogs (ext4 tools); they must be installed via
/// Homebrew. This checks the standard Homebrew keg-only paths first.
pub fn find_e2fsprogs_tool(name: &str) -> String {
    for prefix in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
    ] {
        let path = format!("{prefix}/{name}");
        if Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

/// Create a minimal ext4 image file with the given name and size.
///
/// Returns the path to the created `.img` file.
/// Requires Homebrew `e2fsprogs` (`brew install e2fsprogs`).
pub fn create_test_image(dir: &Path, name: &str, size_mb: u64) -> PathBuf {
    let img = dir.join(format!("{name}.img"));

    let status = Command::new("truncate")
        .args(["-s", &format!("{size_mb}M")])
        .arg(&img)
        .status()
        .expect("failed to run truncate");
    assert!(status.success(), "truncate failed");

    let mkfs = find_e2fsprogs_tool("mkfs.ext4");
    let output = Command::new(&mkfs)
        .args(["-F", "-q"])
        .arg(&img)
        .output()
        .unwrap_or_else(|_| {
            panic!("failed to run {mkfs} — is e2fsprogs installed? (brew install e2fsprogs)")
        });
    assert!(
        output.status.success(),
        "mkfs.ext4 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    img
}

/// Create a minimal ext4 rootfs image file (convenience wrapper).
///
/// Same as `create_test_image(dir, "rootfs", size_mb)`.
pub fn create_test_rootfs(dir: &Path, size_mb: u64) -> PathBuf {
    create_test_image(dir, "rootfs", size_mb)
}

// ---------------------------------------------------------------------------
// Manual VM setup helpers (bypass `ember vm create`)
// ---------------------------------------------------------------------------
//
// These helpers manually construct VM state using APFS clones and JSON files.
// They're useful for testing snapshot, resize, and delete operations without
// requiring the full `ember vm create` pipeline.

/// Register a test image in ember's state directory.
///
/// Copies the `.img` file to `images/data/` and writes a minimal
/// `registry.json`. Uses the `library-{name}-{tag}` naming convention
/// that matches OCI image references.
pub fn register_test_image(state_dir: &Path, name: &str, tag: &str, img_path: &Path) {
    let images_dir = state_dir.join("images").join("data");
    let local_name = format!("library-{name}-{tag}");
    let dest = images_dir.join(format!("{local_name}.img"));

    std::fs::copy(img_path, &dest)
        .unwrap_or_else(|e| panic!("failed to copy image to {}: {e}", dest.display()));

    let registry_path = state_dir.join("images").join("registry.json");
    let size = std::fs::metadata(&dest).unwrap().len();
    let registry = serde_json::json!({
        "images": [{
            "reference": format!("docker.io/library/{name}:{tag}"),
            "local_name": local_name,
            "zvol": dest.to_string_lossy(),
            "size_mib": size / (1024 * 1024),
            "pulled_at": "2024-01-01T00:00:00Z"
        }]
    });
    std::fs::write(
        &registry_path,
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();
}

/// Create a VM by manually setting up the storage layout and state files.
///
/// This bypasses `ember vm create` and instead creates the VM directory,
/// APFS-clones the rootfs via `cp -c`, and writes minimal `vm.json`
/// metadata. Sufficient for testing snapshot, delete, resize, and
/// storage efficiency operations.
pub fn create_test_vm_manual(state_dir: &Path, vm_name: &str, image_name: &str) {
    let images_dir = state_dir.join("images").join("data");
    let local_name = format!("library-{image_name}");
    let src_img = images_dir.join(format!("{local_name}.img"));

    let vm_dir = state_dir.join("vms").join(vm_name);
    std::fs::create_dir_all(vm_dir.join("snapshots")).unwrap();

    // APFS clone the base image → VM rootfs.
    let rootfs = vm_dir.join("rootfs.img");
    let status = Command::new("cp")
        .arg("-c")
        .arg(&src_img)
        .arg(&rootfs)
        .status()
        .expect("failed to run cp -c");
    assert!(
        status.success(),
        "cp -c clone failed — are source and destination on the same APFS volume?"
    );

    // Write minimal VM metadata (vm.json).
    let metadata = serde_json::json!({
        "name": vm_name,
        "id": "00000000-0000-0000-0000-000000000000",
        "status": "created",
        "image": format!("docker.io/library/{image_name}"),
        "cpus": 1,
        "memory_mib": 256,
        "disk_size_gib": 1,
        "kernel_path": "/dev/null",
        "disk_path": rootfs.to_string_lossy(),
        "api_socket": vm_dir.join("ember-vz.sock").to_string_lossy(),
        "created_at": "2024-01-01T00:00:00Z",
        "ssh": { "user": "root", "key": "/dev/null" }
    });
    std::fs::write(
        vm_dir.join("vm.json"),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

/// Set up ember init, create a test image, register it, and create a VM.
///
/// Composite helper: `setup_init()` → `create_test_image()` →
/// `register_test_image()` → `create_test_vm_manual()`.
/// Returns the state directory path.
pub fn setup_with_vm(tmp: &Path, test_name: &str, vm_name: &str) -> PathBuf {
    let state_dir = setup_init(tmp);
    let img = create_test_image(tmp, test_name, 64);
    register_test_image(&state_dir, "testimg", "latest", &img);
    create_test_vm_manual(&state_dir, vm_name, "testimg-latest");
    state_dir
}

// ---------------------------------------------------------------------------
// Process / pipe helpers
// ---------------------------------------------------------------------------

/// Spawn ember-vz with a ready-fd pipe.
///
/// Returns (child, pid, read_file) where read_file is the parent's end of
/// the pipe that ember-vz writes the MAC address to.
pub fn spawn_ember_vz(
    ember_vz: &Path,
    kernel: &Path,
    rootfs: &Path,
    serial_log: &Path,
    boot_args: &str,
) -> (std::process::Child, u32, std::fs::File) {
    let (read_owned, write_owned) = nix::unistd::pipe().expect("pipe failed");
    let read_raw = read_owned.into_raw_fd();
    let write_raw = write_owned.into_raw_fd();
    let write_file = unsafe { std::fs::File::from_raw_fd(write_raw) };
    let write_fd_num = write_file.as_raw_fd();

    let mut cmd = Command::new(ember_vz);
    cmd.args([
        "start",
        "--kernel",
        kernel.to_str().unwrap(),
        "--disk",
        rootfs.to_str().unwrap(),
        "--cpus",
        "1",
        "--memory",
        "256",
        "--boot-args",
        boot_args,
        "--serial-log",
        serial_log.to_str().unwrap(),
        "--ready-fd",
        &write_fd_num.to_string(),
    ]);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Clear CLOEXEC on the write fd so ember-vz inherits it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            let flags = nix::libc::fcntl(write_fd_num, nix::libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::fcntl(
                write_fd_num,
                nix::libc::F_SETFD,
                flags & !nix::libc::FD_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().expect("failed to spawn ember-vz");
    let pid = child.id();

    // Close write end in parent.
    drop(write_file);

    // Wrap read end in a File for the caller.
    let read_file = unsafe { std::fs::File::from_raw_fd(read_raw) };

    (child, pid, read_file)
}

/// Read MAC address from the ready-fd pipe with a timeout.
pub fn read_mac_from_pipe(read_file: std::fs::File, timeout: Duration) -> Option<String> {
    let mut pollfd = nix::libc::pollfd {
        fd: read_file.as_raw_fd(),
        events: nix::libc::POLLIN,
        revents: 0,
    };

    let result = unsafe { nix::libc::poll(&mut pollfd, 1, timeout.as_millis() as i32) };

    if result <= 0 {
        return None;
    }

    let mut reader = BufReader::new(read_file);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }

    let mac = line.trim().to_string();
    if mac.is_empty() {
        None
    } else {
        Some(mac)
    }
}

/// Wait for process to exit within timeout, SIGKILL fallback.
pub fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    eprintln!("Timeout — sending SIGKILL");
                    let _ = child.kill();
                    return child.wait().expect("wait after kill failed");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for process: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem / disk helpers
// ---------------------------------------------------------------------------

/// Parse a numeric value from `dumpe2fs -h` output.
///
/// Looks for a line like `Block count:      524288` and returns the number.
pub fn parse_dumpe2fs_value(output: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    let line = output
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("expected '{prefix}' in dumpe2fs output"));
    line.split(':')
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse '{key}' value: {e}\nline: {line}"))
}

/// Get free space in bytes for the volume containing the given path.
pub fn get_free_space_bytes(path: &Path) -> u64 {
    let output = Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .expect("failed to run df");
    assert!(output.status.success(), "df failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1).expect("df output too short");
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Field 3 is "Available" in 1024-byte blocks.
    let avail_kb: u64 = fields[3].parse().expect("failed to parse df available");
    avail_kb * 1024
}

/// Parse mount point from `hdiutil attach -plist` XML output.
///
/// Looks for the `<key>mount-point</key>` entry and returns the value.
pub fn parse_hdiutil_mount_point(plist_xml: &str) -> Option<String> {
    let marker = "<key>mount-point</key>";
    let idx = plist_xml.find(marker)?;
    let after = &plist_xml[idx + marker.len()..];
    let start_tag = "<string>";
    let end_tag = "</string>";
    let s_start = after.find(start_tag)? + start_tag.len();
    let s_end = after.find(end_tag)?;
    if s_start <= s_end {
        Some(after[s_start..s_end].to_string())
    } else {
        None
    }
}
