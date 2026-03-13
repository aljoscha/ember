//! OCI image pull via `skopeo` + layer extraction.
//!
//! Downloads an OCI image from a container registry and unpacks its
//! layers into a rootfs directory. Uses `skopeo` to copy from the
//! registry to a local OCI layout, then parses the OCI manifest and
//! extracts layers with `tar`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Whether we need fakeroot on macOS (non-root user).
///
/// When running as root, tar/mkfs.ext4 can set ownership natively, so
/// fakeroot is unnecessary. This also avoids the arm64/arm64e DYLD
/// injection incompatibility on newer macOS runners.
///
/// Always returns `false` on non-macOS (fakeroot is never used there).
pub(crate) fn needs_fakeroot() -> bool {
    cfg!(target_os = "macos") && !nix::unistd::geteuid().is_root()
}

/// A parsed OCI image reference.
///
/// Examples of accepted inputs:
///   - `alpine` → `docker.io/library/alpine:latest`
///   - `ubuntu:22.04` → `docker.io/library/ubuntu:22.04`
///   - `myuser/myimage:v1` → `docker.io/myuser/myimage:v1`
///   - `ghcr.io/owner/repo:latest` → as-is
///   - `localhost:5000/myimage:dev` → as-is
#[derive(Debug, Clone, PartialEq)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl ImageReference {
    /// Parse a user-provided image reference string.
    pub fn parse(reference: &str) -> Result<Self> {
        if reference.is_empty() {
            return Err(Error::Image("empty image reference".to_string()));
        }

        // Split off the tag (after the last ':'). Guard against port numbers
        // in the registry (e.g. localhost:5000/foo) by checking for '/'.
        let (name_part, tag) = match reference.rsplit_once(':') {
            Some((name, tag)) if !tag.is_empty() && !tag.contains('/') => (name, tag.to_string()),
            _ => (reference, "latest".to_string()),
        };

        // Split registry from repository. A first path component is treated
        // as a registry if it contains a dot, a colon, or is "localhost".
        let (registry, repository) = match name_part.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            Some(_) => {
                // User/org path without explicit registry (e.g. "myuser/myimage").
                ("docker.io".to_string(), name_part.to_string())
            }
            None => {
                // Bare name like "alpine" → docker.io/library/alpine.
                ("docker.io".to_string(), format!("library/{name_part}"))
            }
        };

        if repository.is_empty() {
            return Err(Error::Image("empty repository name".to_string()));
        }

        Ok(ImageReference {
            registry,
            repository,
            tag,
        })
    }

    /// A filesystem-safe name for this image, suitable for ZFS dataset names.
    ///
    /// Replaces `/` with `-` in the repository path.
    pub fn local_name(&self) -> String {
        let repo = self.repository.replace('/', "-");
        format!("{repo}-{}", self.tag)
    }
}

impl fmt::Display for ImageReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

// ---------------------------------------------------------------------------
// OCI manifest types (minimal, just enough for parsing)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OciIndex {
    manifests: Vec<OciDescriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciDescriptor {
    digest: String,
    media_type: Option<String>,
    platform: Option<OciPlatform>,
}

#[derive(Deserialize)]
struct OciPlatform {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct OciManifest {
    layers: Vec<OciLayerDescriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciLayerDescriptor {
    digest: String,
    #[allow(dead_code)]
    media_type: String,
}

// OCI / Docker media types for manifest list detection.
const MANIFEST_LIST_V2: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

// ---------------------------------------------------------------------------
// Pull implementation
// ---------------------------------------------------------------------------

/// Check whether a required CLI tool is available on `PATH`.
fn check_tool(name: &str) -> Result<()> {
    let output = Command::new("which")
        .arg(name)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "which".to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let hint = install_hint(name);
        return Err(Error::Image(format!(
            "'{name}' is not installed — install it with: {hint}"
        )));
    }
    Ok(())
}

/// Platform-appropriate install command hint for a missing tool.
fn install_hint(name: &str) -> String {
    if cfg!(target_os = "macos") {
        // Some Homebrew tools have a different formula name than the binary.
        let pkg = match name {
            "gtar" => "gnu-tar",
            _ => name,
        };
        format!("`brew install {pkg}`")
    } else {
        format!("`pacman -S {name}` or `apt install {name}`")
    }
}

/// Pull an OCI image and unpack its layers into a rootfs directory.
///
/// Uses `skopeo` to download the image into a local OCI layout, then
/// parses the manifest to find layers and extracts them with `tar`.
///
/// # Arguments
///
/// * `reference` — Parsed image reference.
/// * `dest` — Working directory (must exist). The OCI layout is written
///   to `<dest>/oci/` and the rootfs to `<dest>/rootfs/`.
///
/// # Returns
///
/// Path to the unpacked rootfs directory (`<dest>/rootfs/`).
pub fn pull(reference: &ImageReference, dest: &Path) -> Result<PathBuf> {
    check_tool("skopeo")?;
    if cfg!(target_os = "macos") {
        if needs_fakeroot() {
            check_tool("fakeroot")?;
        }
        check_tool("gtar")?;
    }

    let oci_dir = dest.join("oci");
    let rootfs_dir = dest.join("rootfs");

    // Step 1: skopeo copy from registry to local OCI layout.
    let docker_ref = format!(
        "docker://{}/{}:{}",
        reference.registry, reference.repository, reference.tag
    );
    let oci_ref = format!("oci:{}:{}", oci_dir.display(), reference.tag);

    let mut cmd = Command::new("skopeo");
    cmd.args(["copy"]);

    // On macOS, skopeo defaults to OS "darwin" when resolving multi-arch
    // manifest lists. We always want Linux images for VMs.
    if cfg!(target_os = "macos") {
        cmd.args(["--override-os", "linux"]);
    }

    cmd.args([&docker_ref, &oci_ref]);

    let output = cmd.output().map_err(|e| Error::CommandExec {
        command: "skopeo copy".to_string(),
        source: e,
    })?;
    Error::check_command("skopeo copy", output)?;

    // Step 2: Parse OCI layout and extract layers into rootfs.
    fs::create_dir_all(&rootfs_dir).map_err(|e| Error::Io {
        path: rootfs_dir.clone(),
        source: e,
    })?;

    let layers = resolve_layers(&oci_dir)?;
    for digest in &layers {
        clear_opaque_dirs(&oci_dir, digest, &rootfs_dir)?;
        extract_layer(&oci_dir, digest, &rootfs_dir)?;
        process_whiteouts(&rootfs_dir)?;
    }

    Ok(rootfs_dir)
}

/// Walk the OCI layout index to get the ordered list of layer digests.
///
/// Handles both single-platform manifests and multi-architecture
/// manifest lists (picks `linux/<host-arch>`).
fn resolve_layers(oci_dir: &Path) -> Result<Vec<String>> {
    let index_path = oci_dir.join("index.json");
    let index_str = fs::read_to_string(&index_path).map_err(|e| Error::Io {
        path: index_path.clone(),
        source: e,
    })?;
    let index: OciIndex = serde_json::from_str(&index_str)
        .map_err(|e| Error::Image(format!("failed to parse OCI index.json: {e}")))?;

    if index.manifests.is_empty() {
        return Err(Error::Image(
            "OCI index.json contains no manifests".to_string(),
        ));
    }

    // Read the first referenced blob and check if it's a manifest list
    // or a direct image manifest.
    let desc = &index.manifests[0];
    let blob = read_blob(oci_dir, &desc.digest)?;

    let media_type = desc.media_type.as_deref().unwrap_or("");
    if media_type == MANIFEST_LIST_V2 || media_type == OCI_IMAGE_INDEX {
        // Multi-arch manifest list — find linux/<host-arch>.
        let list: OciIndex = serde_json::from_str(&blob)
            .map_err(|e| Error::Image(format!("failed to parse manifest list: {e}")))?;

        let arch = host_oci_arch();
        let platform_desc = list
            .manifests
            .iter()
            .find(|d| {
                d.platform
                    .as_ref()
                    .is_some_and(|p| p.os == "linux" && p.architecture == arch)
            })
            .ok_or_else(|| Error::Image(format!("no manifest found for linux/{arch}")))?;

        let manifest_blob = read_blob(oci_dir, &platform_desc.digest)?;
        parse_manifest_layers(&manifest_blob)
    } else {
        // Single-platform image manifest (or unknown type — try anyway).
        parse_manifest_layers(&blob)
    }
}

/// Parse an OCI image manifest JSON and return layer digests in order.
fn parse_manifest_layers(manifest_json: &str) -> Result<Vec<String>> {
    let manifest: OciManifest = serde_json::from_str(manifest_json)
        .map_err(|e| Error::Image(format!("failed to parse image manifest: {e}")))?;
    Ok(manifest.layers.iter().map(|l| l.digest.clone()).collect())
}

/// Read a blob from the OCI layout blobs directory.
fn read_blob(oci_dir: &Path, digest: &str) -> Result<String> {
    let path = blob_path(oci_dir, digest)?;
    fs::read_to_string(&path).map_err(|e| Error::Io { path, source: e })
}

/// Compute the filesystem path for a blob given its `algo:hash` digest.
fn blob_path(oci_dir: &Path, digest: &str) -> Result<PathBuf> {
    let (algo, hash) = digest
        .split_once(':')
        .ok_or_else(|| Error::Image(format!("invalid digest format: {digest}")))?;
    Ok(oci_dir.join("blobs").join(algo).join(hash))
}

/// Pre-scan a layer tar for opaque whiteout markers (`.wh..wh..opq`) and
/// clear existing directory contents before extraction.
///
/// OCI opaque whiteouts mean "only the current layer's files should exist
/// in this directory". We must remove previous-layer entries *before*
/// extracting, so the current layer's files are the only ones that remain.
fn clear_opaque_dirs(oci_dir: &Path, digest: &str, rootfs_dir: &Path) -> Result<()> {
    let layer_path = blob_path(oci_dir, digest)?;

    // List tar contents (headers only — fast even for large layers).
    let tar_cmd = if cfg!(target_os = "macos") {
        "gtar"
    } else {
        "tar"
    };
    let output = Command::new(tar_cmd)
        .arg("tf")
        .arg(&layer_path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: format!("{tar_cmd} tf"),
            source: e,
        })?;

    if !output.status.success() {
        // Non-fatal: if listing fails, fall back to the old behavior
        // (opaque marker removed but directory not cleared).
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let path = Path::new(line);
        let is_opaque = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == ".wh..wh..opq");
        if !is_opaque {
            continue;
        }

        // Clear existing entries in the parent directory.
        let rel_dir = path.parent().unwrap_or(Path::new(""));
        let abs_dir = rootfs_dir.join(rel_dir);
        if abs_dir.is_dir() {
            let entries = fs::read_dir(&abs_dir).map_err(|e| Error::Io {
                path: abs_dir.clone(),
                source: e,
            })?;
            for entry in entries {
                let entry = entry.map_err(|e| Error::Io {
                    path: abs_dir.clone(),
                    source: e,
                })?;
                let p = entry.path();
                if p.is_dir() {
                    let _ = fs::remove_dir_all(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }

    Ok(())
}

/// Extract a single layer tar archive into the rootfs directory.
///
/// On macOS, uses `fakeroot` + `gtar` so that ownership metadata from the tar
/// archive is tracked even though non-root can't actually chown files. The
/// fakeroot state is accumulated across layers and later consumed by
/// `mkfs.ext4 -d` to produce an ext4 image with correct ownership.
///
/// On Linux (running as root), plain `tar xf` preserves ownership natively.
fn extract_layer(oci_dir: &Path, digest: &str, rootfs_dir: &Path) -> Result<()> {
    let layer_path = blob_path(oci_dir, digest)?;

    if cfg!(target_os = "macos") {
        let use_fakeroot = needs_fakeroot();
        let state_file = rootfs_dir
            .parent()
            .expect("rootfs_dir has parent")
            .join("fakeroot.state");
        let mut cmd = if use_fakeroot {
            let mut c = Command::new("fakeroot");
            c.arg("-s").arg(&state_file);
            if state_file.exists() {
                c.arg("-i").arg(&state_file);
            }
            c.arg("--").arg("gtar");
            c
        } else {
            Command::new("gtar")
        };
        cmd.arg("xf").arg(&layer_path).arg("-C").arg(rootfs_dir);
        let label = if use_fakeroot {
            "fakeroot gtar xf"
        } else {
            "gtar xf"
        };
        let output = cmd.output().map_err(|e| Error::CommandExec {
            command: label.to_string(),
            source: e,
        })?;
        Error::check_command(label, output)?;
    } else {
        let output = Command::new("tar")
            .arg("xf")
            .arg(&layer_path)
            .arg("-C")
            .arg(rootfs_dir)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "tar xf".to_string(),
                source: e,
            })?;
        Error::check_command("tar xf", output)?;
    }

    Ok(())
}

/// Process OCI whiteout files after extracting a layer.
///
/// OCI layers use special marker files to represent deletions:
///   - `.wh.<name>` means the file `<name>` in that directory was deleted.
///   - `.wh..wh..opq` means the directory is opaque (previous-layer entries
///     were already cleared by [`clear_opaque_dirs`]; this just removes the
///     marker file).
///
/// After processing, both the marker file and the target file are removed.
///
/// Uses a recursive `std::fs` walk instead of shelling out to `find`.
fn process_whiteouts(rootfs_dir: &Path) -> Result<()> {
    // Collect whiteout paths first, then process — avoids modifying the
    // directory tree while iterating over it.
    let mut whiteouts = Vec::new();
    collect_whiteouts(rootfs_dir, &mut whiteouts);

    for whiteout_path in &whiteouts {
        let file_name = match whiteout_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if file_name == ".wh..wh..opq" {
            // Opaque whiteout — would require clearing previous-layer entries.
            // Remove just the marker for now.
            let _ = fs::remove_file(whiteout_path);
            continue;
        }

        if let Some(target_name) = file_name.strip_prefix(".wh.") {
            // Regular whiteout: delete both the marker and the target.
            if let Some(parent) = whiteout_path.parent() {
                let target = parent.join(target_name);
                if target.is_dir() {
                    let _ = fs::remove_dir_all(&target);
                } else {
                    let _ = fs::remove_file(&target);
                }
            }
            let _ = fs::remove_file(whiteout_path);
        }
    }

    Ok(())
}

/// Recursively collect `.wh.*` file paths under `dir`.
fn collect_whiteouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_whiteouts(&path, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with(".wh.") {
                out.push(path);
            }
        }
    }
}

/// Map `std::env::consts::ARCH` to OCI platform architecture names.
fn host_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ImageReference parsing --

    #[test]
    fn parse_simple_name() {
        let r = ImageReference::parse("alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_name_with_tag() {
        let r = ImageReference::parse("ubuntu:22.04").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.tag, "22.04");
    }

    #[test]
    fn parse_user_repo() {
        let r = ImageReference::parse("myuser/myimage:v1").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "myuser/myimage");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn parse_user_repo_no_tag() {
        let r = ImageReference::parse("myuser/myimage").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "myuser/myimage");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_full_reference() {
        let r = ImageReference::parse("docker.io/library/ubuntu:22.04").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.tag, "22.04");
    }

    #[test]
    fn parse_ghcr_reference() {
        let r = ImageReference::parse("ghcr.io/owner/repo:latest").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "owner/repo");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_localhost_with_port() {
        let r = ImageReference::parse("localhost:5000/myimage:dev").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, "dev");
    }

    #[test]
    fn parse_localhost_no_tag() {
        let r = ImageReference::parse("localhost:5000/myimage").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(ImageReference::parse("").is_err());
    }

    #[test]
    fn display_format() {
        let r = ImageReference::parse("alpine").unwrap();
        assert_eq!(r.to_string(), "docker.io/library/alpine:latest");
    }

    #[test]
    fn local_name_simple() {
        let r = ImageReference::parse("alpine:3.19").unwrap();
        assert_eq!(r.local_name(), "library-alpine-3.19");
    }

    #[test]
    fn local_name_user_repo() {
        let r = ImageReference::parse("myuser/myapp:v2").unwrap();
        assert_eq!(r.local_name(), "myuser-myapp-v2");
    }

    // -- OCI layout helpers --

    #[test]
    fn blob_path_format() {
        let path = blob_path(Path::new("/tmp/oci"), "sha256:abc123").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/oci/blobs/sha256/abc123"));
    }

    #[test]
    fn blob_path_invalid_digest() {
        assert!(blob_path(Path::new("/tmp/oci"), "no-colon").is_err());
    }

    #[test]
    fn host_arch_is_nonempty() {
        assert!(!host_oci_arch().is_empty());
    }

    #[test]
    fn parse_manifest_layers_extracts_digests() {
        let manifest = r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:configdigest",
                "size": 100
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "sha256:layer1",
                    "size": 1000
                },
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "sha256:layer2",
                    "size": 2000
                }
            ]
        }"#;

        let layers = parse_manifest_layers(manifest).unwrap();
        assert_eq!(layers, vec!["sha256:layer1", "sha256:layer2"]);
    }

    #[test]
    fn parse_manifest_layers_empty() {
        let manifest = r#"{
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:configdigest",
                "size": 100
            },
            "layers": []
        }"#;

        let layers = parse_manifest_layers(manifest).unwrap();
        assert!(layers.is_empty());
    }

    #[test]
    fn parse_manifest_layers_invalid_json() {
        assert!(parse_manifest_layers("not json").is_err());
    }
}
