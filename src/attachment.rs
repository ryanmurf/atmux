//! Validated image attachments delivered to terminal agents by local path.
//!
//! Browser and agent host clipboards are deliberately not involved. The node
//! that owns a pane writes each image into its private cache and pastes only a
//! generated path into tmux, which works for both local and federated panes.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::tmux::Tmux;

pub const MAX_IMAGE_ATTACHMENTS: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_ATTACHMENT_REQUEST_BODY_BYTES: usize = 17 * 1024 * 1024;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_SCAN_ENTRIES: usize = 1_024;
const ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const IMAGE_PASTE_SETTLE_DELAY: Duration = Duration::from_millis(250);
const NATIVE_IMAGE_PASTE_TIMEOUT: Duration = Duration::from_secs(3);
const NATIVE_IMAGE_PASTE_POLL: Duration = Duration::from_millis(50);
const FILE_PREFIX: &str = "image-";
static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static CACHE_STAGE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncodedImage {
    pub media_type: String,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageMessageRequest {
    #[serde(default)]
    pub text: String,
    pub images: Vec<EncodedImage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryErrorKind {
    Invalid,
    Internal,
}

#[derive(Debug)]
pub struct DeliveryError {
    kind: DeliveryErrorKind,
    source: anyhow::Error,
}

impl DeliveryError {
    #[must_use]
    pub const fn kind(&self) -> DeliveryErrorKind {
        self.kind
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: DeliveryErrorKind::Invalid,
            source: anyhow!(message.into()),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        Self {
            kind: DeliveryErrorKind::Internal,
            source: error,
        }
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}

impl std::error::Error for DeliveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

#[derive(Debug)]
struct ValidatedImage {
    extension: &'static str,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct StagedMessage {
    message: String,
    paths: Vec<PathBuf>,
    retained: bool,
}

impl StagedMessage {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for StagedMessage {
    fn drop(&mut self) {
        if !self.retained {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Validates, stores, and submits an image-bearing message to one local pane.
///
/// Files are retained only after tmux accepts the complete literal message.
///
/// # Errors
///
/// Returns a caller error for malformed images and an internal error for cache
/// or tmux failures.
pub fn deliver(
    pane_id: &str,
    request: ImageMessageRequest,
    wait_for_native_image: bool,
) -> Result<(), DeliveryError> {
    let (text, images) = validate(request)?;
    let root = attachment_root().map_err(DeliveryError::internal)?;
    let staged = stage(&root, &text, &images).map_err(DeliveryError::internal)?;
    if staged.message.len() > MAX_MESSAGE_BYTES {
        return Err(DeliveryError::invalid(format!(
            "message plus image paths exceeds the {MAX_MESSAGE_BYTES}-byte limit"
        )));
    }
    let prior_markers = wait_for_native_image
        .then(|| Tmux.capture(pane_id, 40).ok())
        .flatten()
        .map(|content| native_image_marker_count(&content));
    Tmux.send_text(pane_id, &staged.message, false)
        .context("failed to paste the image message into tmux")
        .map_err(DeliveryError::internal)?;
    if wait_for_native_image {
        wait_for_native_image_conversion(pane_id, prior_markers, images.len());
    } else {
        std::thread::sleep(IMAGE_PASTE_SETTLE_DELAY);
    }
    Tmux.submit(pane_id)
        .context("failed to submit the image message to tmux")
        .map_err(DeliveryError::internal)?;
    staged.retain();
    Ok(())
}

/// Checks caller-controlled attachment data before a pane mutation is claimed.
/// Delivery repeats this validation while holding the pane gate so the
/// validated request and staged files cannot be substituted between steps.
pub(crate) fn validate_request(request: &ImageMessageRequest) -> Result<(), DeliveryError> {
    validate(request.clone()).map(|_| ())
}

fn wait_for_native_image_conversion(
    pane_id: &str,
    prior_markers: Option<usize>,
    expected_images: usize,
) {
    let deadline = Instant::now() + NATIVE_IMAGE_PASTE_TIMEOUT;
    let expected_markers = expected_native_image_marker_count(prior_markers, expected_images);
    loop {
        std::thread::sleep(NATIVE_IMAGE_PASTE_POLL);
        if expected_markers.is_some_and(|expected| {
            Tmux.capture(pane_id, 40)
                .is_ok_and(|content| native_image_marker_count(&content) >= expected)
        }) || Instant::now() >= deadline
        {
            return;
        }
    }
}

fn expected_native_image_marker_count(
    prior_markers: Option<usize>,
    expected_images: usize,
) -> Option<usize> {
    prior_markers.map(|count| count.saturating_add(expected_images))
}

fn native_image_marker_count(content: &str) -> usize {
    content.matches("[Image #").count()
}

fn attachment_root() -> Result<PathBuf> {
    let directories = ProjectDirs::from("dev", "ryanmurf", "atmux")
        .context("could not determine the user cache directory")?;
    Ok(directories.cache_dir().join("attachments"))
}

fn validate(
    request: ImageMessageRequest,
) -> std::result::Result<(String, Vec<ValidatedImage>), DeliveryError> {
    if request.images.is_empty() {
        return Err(DeliveryError::invalid("at least one image is required"));
    }
    if request.images.len() > MAX_IMAGE_ATTACHMENTS {
        return Err(DeliveryError::invalid(format!(
            "at most {MAX_IMAGE_ATTACHMENTS} images may be attached"
        )));
    }
    if request.text.contains('\0') {
        return Err(DeliveryError::invalid("message cannot contain a NUL byte"));
    }
    if request.text.len() > MAX_MESSAGE_BYTES {
        return Err(DeliveryError::invalid(format!(
            "message exceeds the {MAX_MESSAGE_BYTES}-byte limit"
        )));
    }

    let mut total = 0_usize;
    let mut validated = Vec::with_capacity(request.images.len());
    for (index, image) in request.images.into_iter().enumerate() {
        let extension = match image.media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            _ => {
                return Err(DeliveryError::invalid(format!(
                    "image {} must be PNG or JPEG",
                    index + 1
                )));
            }
        };
        let max_encoded_bytes = MAX_IMAGE_BYTES.div_ceil(3) * 4;
        if image.data.is_empty() || image.data.len() > max_encoded_bytes {
            return Err(DeliveryError::invalid(format!(
                "image {} exceeds the {MAX_IMAGE_BYTES}-byte limit",
                index + 1
            )));
        }
        let bytes = STANDARD.decode(&image.data).map_err(|_| {
            DeliveryError::invalid(format!("image {} is not valid base64", index + 1))
        })?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(DeliveryError::invalid(format!(
                "image {} exceeds the {MAX_IMAGE_BYTES}-byte limit",
                index + 1
            )));
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| DeliveryError::invalid("the combined image size is too large"))?;
        if total > MAX_TOTAL_IMAGE_BYTES {
            return Err(DeliveryError::invalid(format!(
                "combined images exceed the {MAX_TOTAL_IMAGE_BYTES}-byte limit"
            )));
        }
        if !signature_matches(extension, &bytes) {
            return Err(DeliveryError::invalid(format!(
                "image {} contents do not match its media type",
                index + 1
            )));
        }
        validated.push(ValidatedImage { extension, bytes });
    }
    Ok((request.text, validated))
}

fn signature_matches(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9]),
        _ => false,
    }
}

fn stage(root: &Path, text: &str, images: &[ValidatedImage]) -> Result<StagedMessage> {
    let _cache_guard = CACHE_STAGE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    prepare_root(root)?;
    cleanup_and_check_capacity(root, images.len())?;
    let mut paths = Vec::with_capacity(images.len());
    for image in images {
        match write_image(root, image) {
            Ok(path) => paths.push(path),
            Err(error) => {
                for path in &paths {
                    let _ = fs::remove_file(path);
                }
                return Err(error);
            }
        }
    }
    let message = match compose_message(text, &paths) {
        Ok(message) => message,
        Err(error) => {
            for path in &paths {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
    };
    Ok(StagedMessage {
        message,
        paths,
        retained: false,
    })
}

fn prepare_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!(
                    "attachment cache must be a real directory: {}",
                    root.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root)
                .with_context(|| format!("failed to create attachment cache {}", root.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect attachment cache {}", root.display()));
        }
    }
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to inspect attachment cache {}", root.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "attachment cache must be a real directory: {}",
            root.display()
        );
    }
    set_private_directory_permissions(root)?;
    Ok(())
}

fn cleanup_and_check_capacity(root: &Path, incoming: usize) -> Result<()> {
    let now = SystemTime::now();
    let mut retained = 0_usize;
    let mut seen = 0_usize;
    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to scan attachment cache {}", root.display()))?
    {
        seen += 1;
        if seen > MAX_CACHE_SCAN_ENTRIES {
            bail!("attachment cache contains too many entries");
        }
        let entry = entry.context("failed to read an attachment cache entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(FILE_PREFIX) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect cached attachment {}", path.display()))?;
        if !metadata.file_type().is_file() {
            if metadata.file_type().is_symlink() {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ATTACHMENT_TTL);
        if expired {
            fs::remove_file(&path).with_context(|| {
                format!("failed to remove expired attachment {}", path.display())
            })?;
        } else {
            retained += 1;
        }
    }
    if retained.saturating_add(incoming) > MAX_CACHE_ENTRIES {
        bail!("attachment cache is full; wait for older images to expire");
    }
    Ok(())
}

fn write_image(root: &Path, image: &ValidatedImage) -> Result<PathBuf> {
    for _ in 0..64 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let suffix = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "{FILE_PREFIX}{timestamp}-{}-{suffix}.{}",
            std::process::id(),
            image.extension
        ));
        let opened = open_private_file(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create attachment {}", path.display()));
            }
        };
        let written = (|| {
            file.write_all(&image.bytes)
                .with_context(|| format!("failed to write attachment {}", path.display()))?;
            file.flush()
                .with_context(|| format!("failed to flush attachment {}", path.display()))?;
            verify_private_file(&file, &path)
        })();
        match written {
            Ok(()) => return Ok(path),
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        }
    }
    bail!("could not allocate a unique attachment file")
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure attachment cache {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_private_file(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect attachment {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
        bail!("attachment file is not private: {}", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_file(file: &File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect attachment {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("attachment path is not a regular file: {}", path.display());
    }
    Ok(())
}

fn compose_message(text: &str, paths: &[PathBuf]) -> Result<String> {
    let noun = if paths.len() == 1 { "image" } else { "images" };
    let mut message = format!(
        "Please inspect the following local {noun} with your image-viewing capability before responding:\n"
    );
    for path in paths {
        let path = path
            .to_str()
            .with_context(|| format!("attachment path is not valid UTF-8: {}", path.display()))?;
        message.push_str("- ");
        message.push_str(path);
        message.push('\n');
    }
    let text = text.trim();
    if text.is_empty() {
        message.push_str("\nDescribe what you see and ask what I would like to do with it if the intent is unclear.");
    } else {
        message.push_str("\nUser message:\n");
        message.push_str(text);
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsmall fixture";
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, b'x', 0xff, 0xd9];

    fn encoded(media_type: &str, bytes: &[u8]) -> EncodedImage {
        EncodedImage {
            media_type: media_type.to_owned(),
            data: STANDARD.encode(bytes),
        }
    }

    fn request(images: Vec<EncodedImage>) -> ImageMessageRequest {
        ImageMessageRequest {
            text: "review this".to_owned(),
            images,
        }
    }

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "atmux-attachment-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn validates_png_and_jpeg_signatures_and_aggregate_limits() {
        let (_, images) = validate(request(vec![
            encoded("image/png", PNG),
            encoded("image/jpeg", JPEG),
        ]))
        .unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].extension, "png");
        assert_eq!(images[1].extension, "jpg");

        let error = validate(request(vec![encoded("image/png", b"not a png")])).unwrap_err();
        assert_eq!(error.kind(), DeliveryErrorKind::Invalid);
        assert!(error.to_string().contains("do not match"));

        let oversized = vec![0_u8; MAX_IMAGE_BYTES + 1];
        let error = validate(request(vec![encoded("image/png", &oversized)])).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn rejects_bad_base64_types_counts_and_empty_requests() {
        assert!(validate(request(Vec::new())).is_err());
        assert!(
            validate(request(vec![encoded("image/gif", b"GIF89a")]))
                .unwrap_err()
                .to_string()
                .contains("PNG or JPEG")
        );
        assert!(
            validate(request(vec![EncodedImage {
                media_type: "image/png".to_owned(),
                data: "%%%".to_owned(),
            }]))
            .unwrap_err()
            .to_string()
            .contains("base64")
        );
        let too_many = (0..=MAX_IMAGE_ATTACHMENTS)
            .map(|_| encoded("image/png", PNG))
            .collect();
        assert!(validate(request(too_many)).is_err());
    }

    #[test]
    fn staging_uses_private_files_and_a_model_readable_prompt() {
        let root = temp_root();
        let (text, images) = validate(request(vec![encoded("image/png", PNG)])).unwrap();
        let mut staged = stage(&root, &text, &images).unwrap();
        assert_eq!(staged.paths.len(), 1);
        assert_eq!(fs::read(&staged.paths[0]).unwrap(), PNG);
        assert!(staged.message.contains("image-viewing capability"));
        assert!(staged.message.contains(staged.paths[0].to_str().unwrap()));
        assert!(staged.message.ends_with("User message:\nreview this"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&staged.paths[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        staged.retained = true;
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_or_abandoned_staging_removes_new_files() {
        let root = temp_root();
        let (text, images) = validate(request(vec![encoded("image/png", PNG)])).unwrap();
        let path = {
            let staged = stage(&root, &text, &images).unwrap();
            let path = staged.paths[0].clone();
            assert!(path.exists());
            path
        };
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_image_conversion_is_detected_only_by_a_new_attachment_marker() {
        assert_eq!(native_image_marker_count("plain prompt"), 0);
        assert_eq!(
            native_image_marker_count("old [Image #1]\nnew [Image #2]"),
            2
        );
        assert_eq!(expected_native_image_marker_count(Some(2), 3), Some(5));
        assert_eq!(expected_native_image_marker_count(None, 3), None);
    }

    #[test]
    fn concurrent_staging_never_exceeds_the_hard_cache_entry_limit() {
        use std::sync::{Arc, Barrier};

        let root = Arc::new(temp_root());
        let workers = MAX_CACHE_ENTRIES.div_ceil(MAX_IMAGE_ATTACHMENTS) + 6;
        let barrier = Arc::new(Barrier::new(workers));
        let handles = (0..workers)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let (_, images) = validate(request(
                        (0..MAX_IMAGE_ATTACHMENTS)
                            .map(|_| encoded("image/png", PNG))
                            .collect(),
                    ))
                    .unwrap();
                    barrier.wait();
                    stage(&root, "review", &images)
                })
            })
            .collect::<Vec<_>>();
        let staged = handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect::<Vec<_>>();
        assert_eq!(staged.len() * MAX_IMAGE_ATTACHMENTS, MAX_CACHE_ENTRIES);
        assert_eq!(
            fs::read_dir(root.as_ref()).unwrap().count(),
            MAX_CACHE_ENTRIES
        );
        drop(staged);
        fs::remove_dir_all(root.as_ref()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cache_root_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let parent = temp_root();
        let target = parent.join("target");
        let root = parent.join("cache");
        fs::create_dir(&target).unwrap();
        let canary = target.join("canary");
        fs::write(&canary, b"safe").unwrap();
        symlink(&target, &root).unwrap();
        let (_, images) = validate(request(vec![encoded("image/png", PNG)])).unwrap();
        let error = stage(&root, "review", &images).unwrap_err();
        assert!(error.to_string().contains("real directory"));
        assert_eq!(fs::read(&canary).unwrap(), b"safe");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        fs::remove_dir_all(parent).unwrap();
    }
}
