//! Build VM images from Dockerfiles.
//!
//! Uses Docker (or Podman) to build a container image, exports its
//! filesystem as a flat tarball, and extracts that into a rootfs
//! directory.  The caller then feeds the rootfs through the same
//! inject → ext4 → storage import pipeline used by [`crate::image::pull`].

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ember_core::backend::ImageToolConfig;
use ember_core::error::{Error, Result};

/// Built-in Dockerfile for a VM-ready Ubuntu image.
pub const DEFAULT_DOCKERFILE: &str = include_str!("../../images/Dockerfile.ubuntu-dev");

// ---------------------------------------------------------------------------
// Name sanitisation
// ---------------------------------------------------------------------------

/// Sanitise a user-provided image name for use as a ZFS dataset component.
///
/// Replaces `:` with `-` (matching how [`ImageReference::local_name`] works).
/// Only alphanumeric, hyphen, underscore, and period are allowed.
pub fn sanitize_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::Image("empty image name".to_string()));
    }

    let sanitized = name.replace(':', "-");

    if !sanitized
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Error::Image(format!(
            "invalid image name '{name}': only alphanumeric, hyphen, underscore, \
             period, and colon are allowed"
        )));
    }

    Ok(sanitized)
}

// ---------------------------------------------------------------------------
// Container tool detection
// ---------------------------------------------------------------------------

/// Detect whether `docker` or `podman` is available.
///
/// Returns the tool name (e.g. `"docker"`).  Prefers Docker when both
/// are installed.
fn detect_container_tool() -> Result<String> {
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
    Err(Error::Image(
        "neither 'docker' nor 'podman' is installed — install one to build images".to_string(),
    ))
}

/// Check that the tar command (and `fakeroot` if non-root) are available.
///
/// fakeroot is needed to preserve file ownership when extracting tar
/// archives as a non-root user. When running as root, tar can set
/// ownership natively.
fn check_fakeroot_tools(config: &ImageToolConfig) -> Result<()> {
    let mut tools: Vec<(&str, String)> = vec![(
        config.tar_command,
        (config.install_hint)(config.tar_command),
    )];
    if config.needs_fakeroot {
        tools.push(("fakeroot", (config.install_hint)("fakeroot")));
    }
    for (tool, hint) in tools {
        let ok = Command::new("which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return Err(Error::Image(format!(
                "'{tool}' is not installed — install it with: {hint}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Build pipeline
// ---------------------------------------------------------------------------

/// Build a container image from a Dockerfile and export its rootfs.
///
/// 1. `docker build` the image
/// 2. `docker create` a throwaway container
/// 3. `docker export` its filesystem to a tarball
/// 4. Clean up the container and image
/// 5. Extract the tarball into `<work_dir>/rootfs/`
///
/// Returns the path to the unpacked rootfs directory.
pub fn build(
    dockerfile: &Path,
    work_dir: &Path,
    name: &str,
    config: &ImageToolConfig,
) -> Result<PathBuf> {
    let tool = detect_container_tool()?;
    if config.needs_fakeroot {
        check_fakeroot_tools(config)?;
    }
    let tag = format!("ember-build-{name}");
    let context_dir = dockerfile.parent().unwrap_or_else(|| Path::new("."));

    // Step 1: Build the container image.
    // Set DOCKER_BUILDKIT=1 so Docker uses BuildKit instead of the deprecated
    // legacy builder.  Harmless for Podman (which ignores it).
    let output = Command::new(&tool)
        .env("DOCKER_BUILDKIT", "1")
        .args(["build", "-t", &tag, "-f"])
        .arg(dockerfile)
        .arg(context_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: format!("{tool} build"),
            source: e,
        })?;
    Error::check_command(&format!("{tool} build"), output)?;

    // Steps 2-4 are wrapped so we always attempt cleanup.
    let result = export_and_extract(&tool, &tag, work_dir, config);

    // Best-effort cleanup of the docker image.
    let _ = Command::new(&tool).args(["rmi", &tag]).output();

    result
}

/// Create a container, export its filesystem, clean up, and extract.
fn export_and_extract(
    tool: &str,
    tag: &str,
    work_dir: &Path,
    config: &ImageToolConfig,
) -> Result<PathBuf> {
    // Create a throwaway container (not started).
    let output = Command::new(tool)
        .args(["create", tag])
        .output()
        .map_err(|e| Error::CommandExec {
            command: format!("{tool} create"),
            source: e,
        })?;
    let output = Error::check_command(&format!("{tool} create"), output)?;
    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Export the container filesystem to a tarball.
    let tarball = work_dir.join("rootfs.tar");
    let export_result = (|| -> Result<()> {
        let output = Command::new(tool)
            .args(["export", "-o"])
            .arg(&tarball)
            .arg(&container_id)
            .output()
            .map_err(|e| Error::CommandExec {
                command: format!("{tool} export"),
                source: e,
            })?;
        Error::check_command(&format!("{tool} export"), output)?;
        Ok(())
    })();

    // Always remove the throwaway container.
    let _ = Command::new(tool).args(["rm", &container_id]).output();

    export_result?;

    // Extract into rootfs directory.
    let rootfs_dir = work_dir.join("rootfs");
    fs::create_dir_all(&rootfs_dir).map_err(|e| Error::Io {
        path: rootfs_dir.clone(),
        source: e,
    })?;

    // When fakeroot is needed, use it with the platform tar command to preserve
    // ownership metadata from the tarball. Without fakeroot, tar as non-root
    // can't chown, and mkfs.ext4 -d later bakes the wrong uid/gid into the
    // ext4 image. When running as root, fakeroot is skipped (root can chown natively).
    if config.needs_fakeroot {
        let state_file = work_dir.join("fakeroot.state");
        let mut cmd = Command::new("fakeroot");
        cmd.arg("-s").arg(&state_file);
        if state_file.exists() {
            cmd.arg("-i").arg(&state_file);
        }
        cmd.arg("--").arg(config.tar_command);
        cmd.arg("xf").arg(&tarball).arg("-C").arg(&rootfs_dir);
        let label = format!("fakeroot {} xf", config.tar_command);
        let output = cmd.output().map_err(|e| Error::CommandExec {
            command: label.clone(),
            source: e,
        })?;
        Error::check_command(&label, output)?;
    } else {
        let label = format!("{} xf", config.tar_command);
        let output = Command::new(config.tar_command)
            .args(["xf"])
            .arg(&tarball)
            .arg("-C")
            .arg(&rootfs_dir)
            .output()
            .map_err(|e| Error::CommandExec {
                command: label.clone(),
                source: e,
            })?;
        Error::check_command(&label, output)?;
    }

    Ok(rootfs_dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_simple_name() {
        assert_eq!(sanitize_name("ubuntu-vm").unwrap(), "ubuntu-vm");
    }

    #[test]
    fn sanitize_name_with_colon() {
        assert_eq!(sanitize_name("ubuntu-vm:24.04").unwrap(), "ubuntu-vm-24.04");
    }

    #[test]
    fn sanitize_empty_name_fails() {
        assert!(sanitize_name("").is_err());
    }

    #[test]
    fn sanitize_invalid_chars_fails() {
        assert!(sanitize_name("my/image").is_err());
        assert!(sanitize_name("my image").is_err());
    }

    #[test]
    fn default_dockerfile_is_nonempty() {
        assert!(DEFAULT_DOCKERFILE.contains("FROM ubuntu:"));
        assert!(DEFAULT_DOCKERFILE.contains("systemd"));
    }
}
