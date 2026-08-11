use anyhow::{ensure, Context, Result};
use base64::Engine as _;
use goose::conversation::message::Message;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(super) const MAX_IMAGE_ATTACHMENTS: usize = 4;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct PendingImage {
    pub path: PathBuf,
    pub data: String,
    pub mime_type: &'static str,
    pub byte_len: usize,
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return etcetera::home_dir().unwrap_or_else(|_| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = etcetera::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 24
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.get(12..16) == Some(b"IHDR")
        && bytes.get(16..20) != Some(&[0, 0, 0, 0])
        && bytes.get(20..24) != Some(&[0, 0, 0, 0])
    {
        return Some("image/png");
    }
    if bytes.len() >= 6 && bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9])
    {
        return Some("image/jpeg");
    }
    if bytes.len() >= 20
        && bytes.starts_with(b"RIFF")
        && bytes.get(8..12) == Some(b"WEBP")
        && matches!(
            bytes.get(12..16),
            Some(b"VP8 ") | Some(b"VP8L") | Some(b"VP8X")
        )
    {
        let declared = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize + 8;
        if declared == bytes.len() {
            return Some("image/webp");
        }
    }
    None
}

fn extension_mime_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

fn load_one(path: &str, working_dir: &Path, governed_root: Option<&Path>) -> Result<PendingImage> {
    let requested = expand_home(path);
    let requested = if requested.is_absolute() {
        requested
    } else {
        working_dir.join(requested)
    };
    let canonical = requested
        .canonicalize()
        .with_context(|| format!("Image does not exist: {}", requested.display()))?;
    if let Some(root) = governed_root {
        ensure!(
            canonical.starts_with(root),
            "Governed sessions cannot attach images outside the authorized workspace: {}",
            root.display()
        );
    }
    let metadata = canonical
        .metadata()
        .with_context(|| format!("Cannot inspect image: {}", canonical.display()))?;
    ensure!(metadata.is_file(), "Not a file: {}", canonical.display());
    ensure!(
        metadata.len() > 0,
        "Image is empty: {}",
        canonical.display()
    );
    ensure!(
        metadata.len() <= MAX_IMAGE_BYTES as u64,
        "Image exceeds the 10 MiB limit: {}",
        canonical.display()
    );

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&canonical)
        .with_context(|| format!("Cannot open image: {}", canonical.display()))?
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Cannot read image: {}", canonical.display()))?;
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "Image exceeds the 10 MiB limit: {}",
        canonical.display()
    );
    let detected = image_mime_type(&bytes).with_context(|| {
        format!(
            "Not a valid PNG, JPEG, or WebP image: {}",
            canonical.display()
        )
    })?;
    let extension = extension_mime_type(&canonical).with_context(|| {
        format!(
            "Image filename must end in .png, .jpg, .jpeg, or .webp: {}",
            canonical.display()
        )
    })?;
    ensure!(
        detected == extension,
        "Image content does not match its filename extension: {}",
        canonical.display()
    );

    let byte_len = bytes.len();
    Ok(PendingImage {
        path: canonical,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: detected,
        byte_len,
    })
}

pub(super) fn load_images(
    paths: &[String],
    working_dir: &Path,
    governed_root: Option<&Path>,
    existing: &[PendingImage],
) -> Result<Vec<PendingImage>> {
    ensure!(!paths.is_empty(), "Usage: /image <path> [path ...]");
    ensure!(
        existing.len() + paths.len() <= MAX_IMAGE_ATTACHMENTS,
        "At most {MAX_IMAGE_ATTACHMENTS} images may be attached to one message"
    );

    let mut seen: HashSet<PathBuf> = existing.iter().map(|image| image.path.clone()).collect();
    let mut total_bytes: usize = existing.iter().map(|image| image.byte_len).sum();
    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let image = load_one(path, working_dir, governed_root)?;
        ensure!(
            seen.insert(image.path.clone()),
            "Image is already attached: {}",
            image.path.display()
        );
        total_bytes = total_bytes.saturating_add(image.byte_len);
        ensure!(
            total_bytes <= MAX_TOTAL_IMAGE_BYTES,
            "Attached images exceed the 20 MiB combined limit"
        );
        loaded.push(image);
    }
    Ok(loaded)
}

pub(super) fn message_with_images(text: &str, pending: &mut Vec<PendingImage>) -> Message {
    let mut message = Message::user().with_text(text);
    for image in pending.drain(..) {
        message = message.with_image(image.data, image.mime_type);
    }
    message
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024 * 1024) as f64)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024_f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goose::conversation::message::MessageContent;
    use tempfile::TempDir;

    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
        bytes.extend_from_slice(b"fixture");
        bytes
    }

    fn webp_bytes() -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8 fixture".to_vec();
        let size = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    #[test]
    fn validates_and_builds_multimodal_user_messages() {
        let temp = TempDir::new().unwrap();
        let png = temp.path().join("screen.png");
        let webp = temp.path().join("screen.webp");
        std::fs::write(&png, png_bytes()).unwrap();
        std::fs::write(&webp, webp_bytes()).unwrap();

        let paths = vec![
            png.file_name().unwrap().to_string_lossy().to_string(),
            webp.file_name().unwrap().to_string_lossy().to_string(),
        ];
        let mut pending = load_images(&paths, temp.path(), None, &[]).unwrap();
        let message = message_with_images("compare these", &mut pending);

        assert!(pending.is_empty());
        assert_eq!(message.content.len(), 3);
        assert!(
            matches!(&message.content[0], MessageContent::Text(text) if text.text == "compare these")
        );
        assert!(
            matches!(&message.content[1], MessageContent::Image(image) if image.mime_type == "image/png")
        );
        assert!(
            matches!(&message.content[2], MessageContent::Image(image) if image.mime_type == "image/webp")
        );
    }

    #[test]
    fn rejects_outside_governed_workspace_and_extension_spoofing_atomically() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let outside = temp.path().join("outside.png");
        std::fs::write(&outside, png_bytes()).unwrap();
        assert!(load_images(
            &[outside.to_string_lossy().to_string()],
            &workspace,
            Some(&workspace),
            &[],
        )
        .unwrap_err()
        .to_string()
        .contains("outside the authorized workspace"));

        #[cfg(unix)]
        {
            let link = workspace.join("linked.png");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(load_images(
                &[link.to_string_lossy().to_string()],
                &workspace,
                Some(&workspace),
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("outside the authorized workspace"));
        }

        let valid = workspace.join("valid.png");
        let spoofed = workspace.join("spoofed.jpg");
        std::fs::write(&valid, png_bytes()).unwrap();
        std::fs::write(&spoofed, png_bytes()).unwrap();
        let result = load_images(
            &[
                valid.to_string_lossy().to_string(),
                spoofed.to_string_lossy().to_string(),
            ],
            &workspace,
            Some(&workspace),
            &[],
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not match its filename extension"));
    }

    #[test]
    fn enforces_count_duplicate_and_size_limits() {
        let temp = TempDir::new().unwrap();
        let png = temp.path().join("screen.png");
        std::fs::write(&png, png_bytes()).unwrap();
        let path = png.to_string_lossy().to_string();
        let existing = load_images(std::slice::from_ref(&path), temp.path(), None, &[]).unwrap();

        assert!(
            load_images(std::slice::from_ref(&path), temp.path(), None, &existing)
                .unwrap_err()
                .to_string()
                .contains("already attached")
        );
        assert!(load_images(
            &["a".into(), "b".into(), "c".into(), "d".into()],
            temp.path(),
            None,
            &existing,
        )
        .unwrap_err()
        .to_string()
        .contains("At most 4"));

        let oversized = temp.path().join("oversized.png");
        let file = File::create(&oversized).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        assert!(load_images(
            &[oversized.to_string_lossy().to_string()],
            temp.path(),
            None,
            &[],
        )
        .unwrap_err()
        .to_string()
        .contains("10 MiB"));
    }
}
