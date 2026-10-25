//! Native, bounded image import. Call these blocking APIs away from UI threads.
//! Only explicit user imports accept outside paths; durable references never do.
use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::{
    AttachmentSource, ImageAttachment, MAX_ATTACHMENTS_PER_TURN, MAX_IMAGE_BYTES, now_ms,
};

const MAX_PIXELS: u64 = 40_000_000;
const MAX_DIMENSION: u32 = 16384;
const MAX_DECODED_BYTES: u64 = 192 * 1024 * 1024;
pub const MAX_PROVIDER_IMAGE_BYTES: usize = 3 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 512 * 1024;

#[derive(Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

pub struct ProviderImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    attachment: ImageAttachment,
    provider: Variant,
    preview: Variant,
}
#[derive(Serialize, Deserialize)]
struct Variant {
    media_type: String,
    sha256: String,
    bytes: u64,
    width: u32,
    height: u32,
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl AttachmentStore {
    pub fn new(artifacts: PathBuf) -> Result<Self> {
        fs::create_dir_all(&artifacts).context("Create image artifact directory")?;
        let artifacts = artifacts.canonicalize()?;
        let root = artifacts.join("images");
        create_directory(&root)?;
        Ok(Self { root })
    }

    /// The host invokes this only for a native picker, drop or pasted-file action.
    pub fn import_file(
        &self,
        session_id: Uuid,
        path: PathBuf,
        source: AttachmentSource,
        cancel: &CancellationToken,
    ) -> Result<ImageAttachment> {
        check_cancel(cancel)?;
        let path = path
            .canonicalize()
            .context("The selected image is no longer available")?;
        let mut file = File::open(&path).context("Open selected image")?;
        ensure!(file.metadata()?.is_file(), "Choose an image file");
        ensure!(
            file.metadata()?.len() <= MAX_IMAGE_BYTES,
            "Image exceeds the 8 MiB import limit"
        );
        let bytes = read_limited(&mut file, MAX_IMAGE_BYTES, cancel)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Image");
        self.import_bytes(session_id, name, &bytes, source, cancel)
    }

    /// Native clipboard formats are decoded and normalized away from the UI thread.
    /// BMP/TIFF are accepted only here, never through agent-supplied file imports.
    pub fn import_clipboard(
        &self,
        session_id: Uuid,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<ImageAttachment> {
        check_cancel(cancel)?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
            "Clipboard image exceeds the 8 MiB limit"
        );
        let _lease = self.decode_lease(cancel)?;
        let format =
            image::guess_format(bytes).context("Clipboard does not contain a supported image")?;
        if matches!(format, ImageFormat::Bmp | ImageFormat::Tiff) {
            let image = decode_bounded(bytes, format, cancel)?;
            let mut encoded = BoundedBuffer {
                inner: Cursor::new(Vec::new()),
                max: MAX_IMAGE_BYTES as usize,
            };
            image
                .write_to(&mut encoded, ImageFormat::Png)
                .context("Clipboard image cannot fit the 8 MiB PNG import limit")?;
            drop(image);
            self.import_bytes_inner(
                session_id,
                "Clipboard image.png",
                &encoded.inner.into_inner(),
                AttachmentSource::Clipboard,
                cancel,
            )
        } else {
            self.import_bytes_inner(
                session_id,
                "Clipboard image",
                bytes,
                AttachmentSource::Clipboard,
                cancel,
            )
        }
    }

    /// Bounded managed image import; decoding is serialized across app processes.
    pub fn import_bytes(
        &self,
        session_id: Uuid,
        name: &str,
        bytes: &[u8],
        source: AttachmentSource,
        cancel: &CancellationToken,
    ) -> Result<ImageAttachment> {
        check_cancel(cancel)?;
        let _lease = self.decode_lease(cancel)?;
        self.import_bytes_inner(session_id, name, bytes, source, cancel)
    }

    fn decode_lease(&self, cancel: &CancellationToken) -> Result<File> {
        directory_without_link(&self.root)?;
        let path = self.root.join(".decode.lock");
        if path.exists() {
            ensure_regular(&path)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            check_cancel(cancel)?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "Another image is still being prepared. Try again shortly."
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn import_bytes_inner(
        &self,
        session_id: Uuid,
        name: &str,
        bytes: &[u8],
        source: AttachmentSource,
        cancel: &CancellationToken,
    ) -> Result<ImageAttachment> {
        check_cancel(cancel)?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
            "Image must contain at most 8 MiB"
        );
        let format = image::guess_format(bytes).context("This file is not a supported image")?;
        let media_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Gif => "image/gif",
            _ => bail!("Choose a PNG, JPEG, WebP or GIF image"),
        };
        let image = decode_bounded(bytes, format, cancel)?;
        let (width, height) = (image.width(), image.height());
        let (provider, provider_bytes) = encode_variant(
            &image,
            4096,
            MAX_PROVIDER_IMAGE_BYTES,
            format == ImageFormat::Png || image.color().has_alpha(),
            cancel,
        )?;
        let (preview, preview_bytes) =
            encode_variant(&image, 256, MAX_PREVIEW_BYTES, true, cancel)?;
        let attachment = ImageAttachment {
            id: Uuid::new_v4(),
            session_id,
            name: safe_name(name),
            media_type: media_type.into(),
            sha256: digest(bytes),
            bytes: bytes.len() as u64,
            width,
            height,
            source,
            imported_at: now_ms(),
        };
        let parent = self.root.join(session_id.to_string());
        directory_without_link(&self.root)?;
        create_directory(&parent)?;
        let staging = Staging(parent.join(format!(".{}.partial", attachment.id)));
        fs::create_dir(&staging.0)?;
        write_new(&staging.0.join("original"), bytes)?;
        write_new(&staging.0.join("provider"), &provider_bytes)?;
        write_new(&staging.0.join("preview.png"), &preview_bytes)?;
        let receipt = Receipt {
            attachment: attachment.clone(),
            provider,
            preview,
        };
        write_new(
            &staging.0.join("receipt.json"),
            &serde_json::to_vec(&receipt)?,
        )?;
        check_cancel(cancel)?;
        fs::rename(&staging.0, parent.join(attachment.id.to_string()))
            .context("Install imported image")?;
        #[cfg(unix)]
        File::open(&parent)?.sync_all()?;
        Ok(attachment)
    }

    pub fn validate(&self, session_id: Uuid, attachments: &[ImageAttachment]) -> Result<()> {
        ensure!(
            attachments.len() <= MAX_ATTACHMENTS_PER_TURN,
            "Attach at most four images per message"
        );
        let mut ids = std::collections::HashSet::new();
        for attachment in attachments {
            ensure!(
                ids.insert(attachment.id),
                "The same image is attached twice"
            );
            self.read_image(session_id, attachment)?;
        }
        Ok(())
    }

    pub fn read_image(&self, session_id: Uuid, attachment: &ImageAttachment) -> Result<Vec<u8>> {
        let (_, directory) = self.receipt(session_id, attachment)?;
        verified_bytes(
            &directory.join("original"),
            attachment.bytes,
            &attachment.sha256,
            MAX_IMAGE_BYTES,
        )
    }

    pub fn read_provider_image(
        &self,
        session_id: Uuid,
        attachment: &ImageAttachment,
    ) -> Result<ProviderImage> {
        let (receipt, directory) = self.receipt(session_id, attachment)?;
        // Detect deleted, replaced or corrupted originals as well as variants.
        verified_bytes(
            &directory.join("original"),
            attachment.bytes,
            &attachment.sha256,
            MAX_IMAGE_BYTES,
        )?;
        let bytes = verified_bytes(
            &directory.join("provider"),
            receipt.provider.bytes,
            &receipt.provider.sha256,
            MAX_PROVIDER_IMAGE_BYTES as u64,
        )?;
        ensure!(
            matches!(
                receipt.provider.media_type.as_str(),
                "image/png" | "image/jpeg"
            ),
            "Invalid managed image media type"
        );
        Ok(ProviderImage {
            media_type: receipt.provider.media_type,
            bytes,
            width: receipt.provider.width,
            height: receipt.provider.height,
        })
    }

    pub fn preview_path(&self, attachment: &ImageAttachment) -> Result<PathBuf> {
        let (receipt, directory) = self.receipt(attachment.session_id, attachment)?;
        let path = directory.join("preview.png");
        verified_bytes(
            &path,
            receipt.preview.bytes,
            &receipt.preview.sha256,
            MAX_PREVIEW_BYTES as u64,
        )?;
        Ok(path)
    }

    fn receipt(
        &self,
        session_id: Uuid,
        attachment: &ImageAttachment,
    ) -> Result<(Receipt, PathBuf)> {
        directory_without_link(&self.root)?;
        ensure!(
            attachment.session_id == session_id,
            "Image belongs to another conversation"
        );
        ensure!(
            attachment.bytes > 0 && attachment.bytes <= MAX_IMAGE_BYTES,
            "Invalid image reference size"
        );
        ensure!(
            attachment.sha256.len() == 64
                && attachment
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Invalid image reference hash"
        );
        let parent = self.root.join(session_id.to_string());
        directory_without_link(&parent)?;
        let directory = parent.join(attachment.id.to_string());
        directory_without_link(&directory)?;
        let path = directory.join("receipt.json");
        ensure_regular(&path)?;
        let bytes = read_limited(&mut File::open(path)?, 16 * 1024, &CancellationToken::new())?;
        let receipt: Receipt = serde_json::from_slice(&bytes)
            .context("Image receipt is invalid; attach the image again")?;
        ensure!(
            receipt.attachment == *attachment,
            "Image reference does not match the imported image"
        );
        Ok((receipt, directory))
    }
}

fn decode_bounded(
    bytes: &[u8],
    format: ImageFormat,
    cancel: &CancellationToken,
) -> Result<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("Read image header")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Image exceeds the 40 megapixel decode limit"
    );
    ensure!(
        decoder.total_bytes() <= MAX_DECODED_BYTES,
        "Image exceeds the decoded memory limit"
    );
    let orientation = decoder.orientation()?;
    check_cancel(cancel)?;
    let mut image = DynamicImage::from_decoder(decoder).context("Decode image")?;
    image.apply_orientation(orientation);
    check_cancel(cancel)?;
    Ok(image)
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Image import cancelled");
    Ok(())
}
fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect();
    if name.is_empty() {
        "Image".into()
    } else {
        name
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    directory_without_link(path)
}
fn directory_without_link(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("Imported image is missing; attach it again")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Managed image directory must not be a symbolic link"
    );
    Ok(())
}
fn ensure_regular(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("Imported image is missing; attach it again")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Managed image must be a regular file"
    );
    Ok(())
}
fn read_limited(file: &mut File, max: u64, cancel: &CancellationToken) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        ensure!(
            bytes.len() as u64 + n as u64 <= max,
            "Image file exceeds its size limit"
        );
        bytes.extend_from_slice(&buffer[..n]);
    }
    Ok(bytes)
}
fn verified_bytes(path: &Path, size: u64, hash: &str, max: u64) -> Result<Vec<u8>> {
    ensure!(size > 0 && size <= max, "Invalid managed image size");
    ensure_regular(path)?;
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == size,
        "Imported image changed; attach it again"
    );
    let bytes = read_limited(&mut file, max, &CancellationToken::new())?;
    ensure!(
        digest(&bytes) == hash,
        "Imported image failed its integrity check; attach it again"
    );
    Ok(bytes)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

struct BoundedBuffer {
    inner: Cursor<Vec<u8>>,
    max: usize,
}
impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.inner.position() as usize + bytes.len() > self.max {
            return Err(std::io::Error::other("Image encoding exceeds its limit"));
        }
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for BoundedBuffer {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}
fn encode_variant(
    image: &DynamicImage,
    mut edge: u32,
    max: usize,
    png: bool,
    cancel: &CancellationToken,
) -> Result<(Variant, Vec<u8>)> {
    edge = edge.min(image.width().max(image.height()));
    loop {
        check_cancel(cancel)?;
        let resized = image.thumbnail(edge, edge);
        let mut output = BoundedBuffer {
            inner: Cursor::new(Vec::new()),
            max,
        };
        let encoded = if png {
            resized.write_to(&mut output, ImageFormat::Png)
        } else {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 90)
                .encode_image(&resized)
        };
        if encoded.is_ok() {
            let bytes = output.inner.into_inner();
            return Ok((
                Variant {
                    media_type: if png { "image/png" } else { "image/jpeg" }.into(),
                    sha256: digest(&bytes),
                    bytes: bytes.len() as u64,
                    width: resized.width(),
                    height: resized.height(),
                },
                bytes,
            ));
        }
        ensure!(
            edge > 64,
            "Could not encode this image within the transmission limit"
        );
        edge = (edge / 2).max(64);
    }
}

#[cfg(test)]
mod tests;
