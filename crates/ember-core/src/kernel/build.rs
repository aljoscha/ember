//! Container-based kernel build for ember microVMs.
//!
//! Builds a custom Linux kernel with Docker networking and AVF (Apple
//! Virtualization Framework) support inside a container. All build assets
//! (Dockerfile, config fragments, URLs) are embedded in the binary — no
//! runtime dependency on the `kernel/` directory.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context};

use super::KernelPreset;
use crate::state::store::StateStore;

// ---------------------------------------------------------------------------
// Embedded build constants (from kernel/ directory)
// ---------------------------------------------------------------------------

const KERNEL_TAG: &str = "microvm-kernel-6.1.163-20.299.amzn2023";
const KERNEL_REPO: &str = "https://github.com/amazonlinux/linux.git";
const BASE_CONFIG_URL_TEMPLATE: &str = "https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-{ARCH}-6.1.config";
const BUILDER_IMAGE: &str = "ember-kernel-builder";

const DOCKERFILE: &str = include_str!("../../../../kernel/Dockerfile");
const DOCKER_FRAGMENT: &str = include_str!("../../../../kernel/docker.fragment");
const AVF_FRAGMENT: &str = include_str!("../../../../kernel/avf.fragment");

/// Architecture-specific kernel build parameters.
///
/// Mirrors the `ARCH`/`FC_ARCH`/`KARCH` handling in `kernel/Makefile`: ember
/// builds the kernel for the architecture of the host it runs on, since the VM
/// backend (Firecracker on Linux, AVF on macOS) only boots same-arch guests.
struct TargetArch {
    /// Firecracker CI config arch component, also used as the Docker
    /// `--platform` value (e.g. "aarch64" → config `...-aarch64-...` and
    /// `--platform linux/aarch64`).
    firecracker_arch: &'static str,
    /// Make variable prefix spliced before the `make` target. Empty for native
    /// x86; sets `ARCH`/`CROSS_COMPILE` for arm64. Has a trailing space so it
    /// concatenates cleanly (e.g. `make {cross}olddefconfig`).
    cross: &'static str,
    /// Make target: x86_64 yields an ELF `vmlinux`; arm64 yields the raw
    /// `Image` that VZLinuxBootLoader and Firecracker expect.
    make_target: &'static str,
    /// Built-kernel path relative to the cloned kernel source tree.
    output_rel: &'static str,
}

/// Resolve build parameters for the architecture ember is running on.
fn target_arch() -> anyhow::Result<TargetArch> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(TargetArch {
            firecracker_arch: "x86_64",
            cross: "",
            make_target: "vmlinux",
            output_rel: "vmlinux",
        }),
        "aarch64" => Ok(TargetArch {
            firecracker_arch: "aarch64",
            cross: "ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- ",
            make_target: "Image",
            output_rel: "arch/arm64/boot/Image",
        }),
        other => bail!(
            "unsupported host architecture '{other}' for kernel build \
             (expected x86_64 or aarch64)"
        ),
    }
}

/// Firecracker config architecture for the host ember runs on (`x86_64` or
/// `aarch64`). Exposed so the CLI can show the build target in its prompt
/// without reaching into the build internals.
pub fn host_config_arch() -> anyhow::Result<&'static str> {
    Ok(target_arch()?.firecracker_arch)
}

// ---------------------------------------------------------------------------
// Container tool detection
// ---------------------------------------------------------------------------

/// Detect whether `docker` or `podman` is available.
pub fn detect_container_tool() -> anyhow::Result<String> {
    for tool in &["docker", "podman"] {
        let ok = Command::new("which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok((*tool).to_string());
        }
    }
    bail!(
        "neither 'docker' nor 'podman' is installed.\n\
         Install one to build kernels."
    );
}

// ---------------------------------------------------------------------------
// Build orchestration
// ---------------------------------------------------------------------------

/// Build a kernel with Docker networking and AVF support inside a container.
///
/// 1. Writes build assets (Dockerfile, docker.fragment, avf.fragment) into `work_dir`
/// 2. Builds the builder container image
/// 3. Runs the full kernel build inside the container
/// 4. Copies the resulting vmlinux to the state store's kernels/ directory
///
/// Returns the path to the installed kernel.
pub fn build(store: &StateStore, jobs: usize, tool: &str) -> anyhow::Result<PathBuf> {
    let arch = target_arch()?;
    let platform = format!("linux/{}", arch.firecracker_arch);

    let work_dir = tempfile::tempdir().context("failed to create temp build directory")?;
    let work = work_dir.path();

    println!("Build directory: {}", work.display());

    // Write embedded assets into the work directory.
    std::fs::write(work.join("Dockerfile"), DOCKERFILE).context("failed to write Dockerfile")?;
    std::fs::write(work.join("docker.fragment"), DOCKER_FRAGMENT)
        .context("failed to write docker.fragment")?;
    std::fs::write(work.join("avf.fragment"), AVF_FRAGMENT)
        .context("failed to write avf.fragment")?;

    // Build the builder image.
    println!("Building container image ({BUILDER_IMAGE}, {platform})...");
    let output = Command::new(tool)
        .env("DOCKER_BUILDKIT", "1")
        .args(["build", "--platform", &platform, "-t", BUILDER_IMAGE, "."])
        .current_dir(work)
        .output()
        .with_context(|| format!("failed to execute '{tool} build'"))?;
    if !output.status.success() {
        bail!(
            "{tool} build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Run the kernel build inside the container.
    //
    // The build script mirrors the Makefile targets:
    //   1. Download Firecracker CI base config
    //   2. Shallow-clone the Amazon Linux kernel source
    //   3. Merge base config + docker.fragment + avf.fragment
    //   4. Strip BUILD_SALT for reproducibility
    //   5. Compile vmlinux
    let uid = nix::unistd::getuid();
    let gid = nix::unistd::getgid();
    let user_flag = format!("{}:{}", uid, gid);

    let base_config_url = BASE_CONFIG_URL_TEMPLATE.replace("{ARCH}", arch.firecracker_arch);
    let cross = arch.cross;
    let make_target = arch.make_target;
    let firecracker_arch = arch.firecracker_arch;

    let build_script = format!(
        "set -e\n\
         echo '==> Downloading Firecracker CI kernel config ({firecracker_arch})...'\n\
         curl -fSL -o base.config '{base_config_url}'\n\
         echo '==> Cloning kernel source (shallow, tag {KERNEL_TAG})...'\n\
         git clone --depth 1 --branch '{KERNEL_TAG}' '{KERNEL_REPO}' linux\n\
         echo '==> Merging base config + fragments...'\n\
         cd linux\n\
         KCONFIG_CONFIG=.config scripts/kconfig/merge_config.sh -m ../base.config ../docker.fragment ../avf.fragment\n\
         sed -i 's/^CONFIG_BUILD_SALT=.*/CONFIG_BUILD_SALT=\"\"/' .config\n\
         make {cross}olddefconfig\n\
         echo '==> Building {make_target} ({jobs} jobs)...'\n\
         make -j{jobs} {cross}{make_target}\n\
         echo '==> Done.'"
    );

    println!("Starting kernel build (this may take 10-30 minutes)...");
    let status = Command::new(tool)
        .args([
            "run",
            "--rm",
            "--platform",
            &platform,
            "--user",
            &user_flag,
            "-v",
            &format!("{}:/build", work.display()),
            "-w",
            "/build",
            BUILDER_IMAGE,
            "sh",
            "-c",
            &build_script,
        ])
        .status()
        .with_context(|| format!("failed to execute '{tool} run'"))?;
    if !status.success() {
        bail!(
            "kernel build failed (exit code {})",
            status.code().unwrap_or(-1)
        );
    }

    // Copy the built kernel to the state store.
    let built = work.join("linux").join(arch.output_rel);
    if !built.exists() {
        bail!(
            "build completed but vmlinux not found at {}",
            built.display()
        );
    }

    let kernel_dir = store.kernel_dir();
    std::fs::create_dir_all(&kernel_dir)
        .with_context(|| format!("failed to create {}", kernel_dir.display()))?;

    let dest = kernel_dir.join(KernelPreset::Docker.filename());
    std::fs::copy(&built, &dest).with_context(|| {
        format!(
            "failed to copy vmlinux from {} to {}",
            built.display(),
            dest.display()
        )
    })?;

    println!("Kernel installed to {}", dest.display());
    Ok(dest)
}
