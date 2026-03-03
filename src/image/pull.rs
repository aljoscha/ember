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
            Some((name, tag)) if !tag.is_empty() && !tag.contains('/') => {
                (name, tag.to_string())
            }
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
        return Err(Error::Image(format!(
            "'{name}' is not installed — install it with your package manager \
             (e.g. `pacman -S {name}` or `apt install {name}`)"
        )));
    }
    Ok(())
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

    let oci_dir = dest.join("oci");
    let rootfs_dir = dest.join("rootfs");

    // Step 1: skopeo copy from registry to local OCI layout.
    let docker_ref = format!(
        "docker://{}/{}:{}",
        reference.registry, reference.repository, reference.tag
    );
    let oci_ref = format!("oci:{}:{}", oci_dir.display(), reference.tag);

    let output = Command::new("skopeo")
        .args(["copy", &docker_ref, &oci_ref])
        .output()
        .map_err(|e| Error::CommandExec {
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
            .ok_or_else(|| {
                Error::Image(format!("no manifest found for linux/{arch}"))
            })?;

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
    let (algo, hash) = digest.split_once(':').ok_or_else(|| {
        Error::Image(format!("invalid digest format: {digest}"))
    })?;
    Ok(oci_dir.join("blobs").join(algo).join(hash))
}

/// Extract a single layer tar archive into the rootfs directory.
///
/// Uses GNU tar which auto-detects compression (gzip, zstd, xz, etc.).
fn extract_layer(oci_dir: &Path, digest: &str, rootfs_dir: &Path) -> Result<()> {
    let layer_path = blob_path(oci_dir, digest)?;

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
    Ok(())
}

/// Process OCI whiteout files after extracting a layer.
///
/// OCI layers use special marker files to represent deletions:
///   - `.wh.<name>` means the file `<name>` in that directory was deleted.
///   - `.wh..wh..opq` means the directory is opaque (only current layer
///     contents should remain). Opaque whiteouts are not yet handled.
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
