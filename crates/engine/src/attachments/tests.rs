use super::*;
use xylo_core::{AttachmentSource, ImageAttachment};

fn png() -> Vec<u8> {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(24, 16)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    png.into_inner()
}

fn imported() -> (tempfile::TempDir, AttachmentStore, Uuid, ImageAttachment) {
    let directory = tempfile::tempdir().unwrap();
    let store = AttachmentStore::new(directory.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4();
    let attachment = store
        .import_bytes(
            session,
            "fixture.png",
            &png(),
            AttachmentSource::Picker,
            &CancellationToken::new(),
        )
        .unwrap();
    (directory, store, session, attachment)
}

fn conversation(attachment: &ImageAttachment) -> Vec<Message> {
    let mut message = Message::user("Look");
    message.attachments = vec![attachment.clone()];
    vec![message, Message::assistant("Seen"), Message::user("Again")]
}

/// Overwrites every stored copy of the image (original and variants).
fn corrupt(directory: &std::path::Path) {
    for entry in walk(directory) {
        let name = entry.file_name().unwrap().to_string_lossy().into_owned();
        if name == "original" || name == "provider" {
            let len = std::fs::metadata(&entry).unwrap().len() as usize;
            std::fs::write(&entry, vec![0u8; len]).unwrap();
        }
    }
}

fn walk(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn an_image_is_verified_once_per_turn_and_reused_for_later_calls() {
    let (directory, store, session, attachment) = imported();
    let encoded = Mutex::new(HashMap::new());
    let cancel = CancellationToken::new();
    let mut first = conversation(&attachment);
    hydrate(&store, session, &encoded, &mut first, &cancel).unwrap();
    let expected = STANDARD.encode(
        store
            .read_provider_image(session, &attachment)
            .unwrap()
            .bytes,
    );
    assert_eq!(first[0].images.len(), 1);
    assert_eq!(first[0].images[0].data, expected);

    // Later calls of the same turn do not touch the files again.
    corrupt(directory.path());
    let mut later = conversation(&attachment);
    hydrate(&store, session, &encoded, &mut later, &cancel).unwrap();
    assert_eq!(later[0].images, first[0].images);

    // A new turn verifies again, and fails closed on the damaged files.
    let next_turn = Mutex::new(HashMap::new());
    let mut next = conversation(&attachment);
    assert!(hydrate(&store, session, &next_turn, &mut next, &cancel).is_err());
}

#[test]
fn the_first_use_still_checks_integrity() {
    let (directory, store, session, attachment) = imported();
    corrupt(directory.path());
    let mut messages = conversation(&attachment);
    let error = hydrate(
        &store,
        session,
        &Mutex::new(HashMap::new()),
        &mut messages,
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(error, ModelError::InvalidRequest(_)), "{error:?}");
}
