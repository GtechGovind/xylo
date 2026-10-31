use super::*;
use image::{GenericImageView, ImageBuffer, Rgb};
use xylo_core::{PermissionMode, ProviderConfig, TurnRequest};

fn encoded(format: ImageFormat) -> Vec<u8> {
    let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(48, 32, |x, y| {
        Rgb([x as u8, y as u8, 180])
    }));
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, format).unwrap();
    output.into_inner()
}
#[test]
fn actual_native_formats_are_sniffed_copied_and_previewed_with_durable_refs() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    for format in [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::WebP,
        ImageFormat::Gif,
    ] {
        let bytes = encoded(format);
        let selected = temp.path().join("wrong-extension.txt");
        fs::write(&selected, &bytes).unwrap();
        let reference = store
            .import_file(
                session,
                selected,
                AttachmentSource::Picker,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!((reference.width, reference.height), (48, 32));
        assert_eq!(reference.sha256, digest(&bytes));
        assert_eq!(store.read_image(session, &reference).unwrap(), bytes);
        let preview = store.preview_path(&reference).unwrap();
        assert!(preview.metadata().unwrap().len() <= MAX_PREVIEW_BYTES as u64);
        let preview = image::open(preview).unwrap();
        assert!(preview.width() <= 256 && preview.height() <= 256);
        let provider = store.read_provider_image(session, &reference).unwrap();
        assert!(provider.bytes.len() <= MAX_PROVIDER_IMAGE_BYTES);
        assert!(matches!(
            provider.media_type.as_str(),
            "image/png" | "image/jpeg"
        ));
        assert_eq!(
            image::load_from_memory(&provider.bytes)
                .unwrap()
                .dimensions(),
            (48, 32)
        );
        let json = serde_json::to_string(&reference).unwrap();
        assert!(!json.contains("base64"));
        assert!(!json.contains(temp.path().to_str().unwrap()));
        assert!(json.len() < 1024);
    }
}
#[test]
fn references_cannot_cross_sessions_or_hide_tampering_or_oversized_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    let cancel = CancellationToken::new();
    let reference = store
        .import_bytes(
            session,
            "test.png",
            &encoded(ImageFormat::Png),
            AttachmentSource::Clipboard,
            &cancel,
        )
        .unwrap();
    assert!(store.read_image(Uuid::new_v4(), &reference).is_err());
    let mut altered = reference.clone();
    altered.name = "changed reference".into();
    assert!(store.preview_path(&altered).is_err());
    let mut duplicates = vec![reference.clone(); 2];
    assert!(store.validate(session, &duplicates).is_err());
    duplicates.clear();
    let mut huge = encoded(ImageFormat::Jpeg);
    let sof = huge
        .windows(2)
        .position(|bytes| bytes == [0xff, 0xc0])
        .unwrap();
    huge[sof + 5..sof + 7].copy_from_slice(&9999u16.to_be_bytes());
    huge[sof + 7..sof + 9].copy_from_slice(&9999u16.to_be_bytes());
    assert!(
        store
            .import_bytes(session, "bomb.jpg", &huge, AttachmentSource::Drop, &cancel)
            .is_err()
    );
    assert!(
        store
            .import_bytes(
                session,
                "script.png",
                b"<script>not an image</script>",
                AttachmentSource::Drop,
                &cancel
            )
            .is_err()
    );
    assert!(
        store
            .import_bytes(
                session,
                "large.png",
                &vec![0; MAX_IMAGE_BYTES as usize + 1],
                AttachmentSource::Drop,
                &cancel
            )
            .is_err()
    );
    let original = store
        .root
        .join(session.to_string())
        .join(reference.id.to_string())
        .join("original");
    fs::write(original, b"corruption").unwrap();
    assert!(store.read_provider_image(session, &reference).is_err());
}
#[test]
fn cancelled_imports_leave_no_partial_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        store
            .import_bytes(
                Uuid::new_v4(),
                "cancel.png",
                &encoded(ImageFormat::Png),
                AttachmentSource::Picker,
                &cancel
            )
            .is_err()
    );
    assert_eq!(fs::read_dir(&store.root).unwrap().count(), 0);
}
#[test]
fn user_turn_reopen_preserves_refs_and_idempotency_rejects_changed_images() {
    let temp = tempfile::tempdir().unwrap();
    let assets = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    let reference = assets
        .import_bytes(
            session,
            "resumable.png",
            &encoded(ImageFormat::Png),
            AttachmentSource::PastedFile,
            &CancellationToken::new(),
        )
        .unwrap();
    let mut turn = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: String::new(),
        workspace: temp.path().into(),
        provider: ProviderConfig::default(),
        permissions: PermissionMode::ReadOnly,
        attachments: vec![reference.clone()],
        approved_rules: Vec::new(),
    };
    let database = temp.path().join("history.sqlite");
    let store = crate::Store::open(&database).unwrap();
    store.register_user_turn(&turn).unwrap();
    store.register_user_turn(&turn).unwrap();
    drop(store);
    let reopened = crate::Store::open(&database).unwrap();
    let messages = reopened.messages(session, 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].attachments, vec![reference]);
    assert!(messages[0].content.is_empty());
    turn.attachments.clear();
    assert!(reopened.register_user_turn(&turn).is_err());
    assert_eq!(reopened.messages(session, 10).unwrap().len(), 1);
    let wire = serde_json::to_vec(&xylo_core::Envelope::new(
        turn.turn_id,
        Some(session),
        0,
        turn,
    ))
    .unwrap();
    assert!(wire.len() < 4096);
}
#[cfg(unix)]
#[test]
fn symbolic_links_cannot_redirect_managed_reads() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    let reference = store
        .import_bytes(
            session,
            "test.png",
            &encoded(ImageFormat::Png),
            AttachmentSource::Picker,
            &CancellationToken::new(),
        )
        .unwrap();
    let path = store
        .root
        .join(session.to_string())
        .join(reference.id.to_string())
        .join("provider");
    let outside = temp.path().join("outside");
    fs::copy(&path, &outside).unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(outside, path).unwrap();
    assert!(store.read_provider_image(session, &reference).is_err());
}

#[test]
fn native_clipboard_bmp_and_tiff_are_normalized_and_keep_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    for format in [ImageFormat::Bmp, ImageFormat::Tiff, ImageFormat::Png] {
        let bytes = encoded(format);
        assert!(
            format == ImageFormat::Png
                || store
                    .import_bytes(
                        session,
                        "file",
                        &bytes,
                        AttachmentSource::Drop,
                        &CancellationToken::new()
                    )
                    .is_err()
        );
        let image = store
            .import_clipboard(session, &bytes, &CancellationToken::new())
            .unwrap();
        assert_eq!(image.source, AttachmentSource::Clipboard);
        assert_eq!(image.media_type, "image/png");
        let saved = store.read_image(session, &image).unwrap();
        assert_eq!(image::guess_format(&saved).unwrap(), ImageFormat::Png);
        assert_eq!(
            image::load_from_memory(&saved).unwrap().dimensions(),
            (48, 32)
        );
    }
    assert!(
        store
            .import_clipboard(session, b"<svg></svg>", &CancellationToken::new())
            .is_err()
    );
}

#[test]
fn cross_process_decode_lease_wait_can_be_cancelled_without_installing_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(temp.path().join("artifacts")).unwrap();
    let token = CancellationToken::new();
    let lease = store.decode_lease(&token).unwrap();
    let worker = store.clone();
    let cancel = token.clone();
    let started = std::time::Instant::now();
    let task = std::thread::spawn(move || {
        worker.import_clipboard(Uuid::new_v4(), &encoded(ImageFormat::Png), &cancel)
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    token.cancel();
    assert!(
        task.join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    drop(lease);
    assert_eq!(fs::read_dir(&store.root).unwrap().count(), 1);
}
