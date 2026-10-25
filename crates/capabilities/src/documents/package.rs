use super::xml;
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{Cursor, Write},
    path::Path,
};
use tokio_util::sync::CancellationToken;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};
pub struct Package {
    archive: ZipArchive<Cursor<Vec<u8>>>,
}
impl Package {
    pub fn new(bytes: Vec<u8>, cancel: &CancellationToken) -> Result<Self> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))?;
        ensure!(
            archive.len() <= 10_000,
            "Office package exceeds 10000 parts"
        );
        let mut total = 0u64;
        let mut names = HashSet::new();
        for i in 0..archive.len() {
            super::check(cancel)?;
            let file = archive.by_index(i)?;
            let name = file.name();
            ensure!(
                !name.to_ascii_lowercase().contains("_xmlsignatures"),
                "Digitally signed Office packages require a signature-aware editor; native editing is unavailable"
            );
            ensure!(
                names.insert(name.to_owned())
                    && !name.starts_with('/')
                    && !name.contains('\\')
                    && !name.split('/').any(|p| p == ".."),
                "Office package has duplicate or unsafe part paths"
            );
            ensure!(
                !file.encrypted() && file.unix_mode().is_none_or(|mode| mode & 0xf000 != 0xa000),
                "Encrypted/symbolic-link Office parts are unsupported"
            );
            total = total
                .checked_add(file.size())
                .context("Office size overflow")?;
            ensure!(
                file.size() <= 32 * 1024 * 1024 && total <= 128 * 1024 * 1024,
                "Office decompression budget exceeded"
            );
            ensure!(
                file.size()
                    <= file
                        .compressed_size()
                        .saturating_mul(1000)
                        .saturating_add(1024 * 1024),
                "Office compression ratio exceeds safe budget"
            );
        }
        let mut package = Self { archive };
        if package.contains("_rels/.rels") {
            let relationships = package.text("_rels/.rels", cancel)?;
            let root = xml::parse(&relationships, cancel)?;
            ensure!(
                !root.children.iter().any(|relationship| relationship
                    .attr("Type")
                    .is_some_and(|kind| kind.contains("digital-signature/origin")
                        || kind.contains("digital-signature/signature"))),
                "Digitally signed Office packages cannot be edited without invalidating their signatures"
            );
        }
        Ok(package)
    }
    pub fn contains(&self, name: &str) -> bool {
        self.archive.file_names().any(|n| n == name)
    }
    pub fn text(&mut self, name: &str, cancel: &CancellationToken) -> Result<String> {
        let mut file = self
            .archive
            .by_name(name)
            .with_context(|| format!("Missing Office part {name}"))?;
        let bytes = super::read_bounded(&mut file, 32 * 1024 * 1024, cancel)?;
        let text = String::from_utf8(bytes).context("Office XML must be UTF-8")?;
        ensure!(
            !text.contains("<!DOCTYPE") && !text.contains("<!ENTITY"),
            "Office entity declarations are unsupported"
        );
        Ok(text)
    }
    pub fn rewrite(
        &mut self,
        path: &Path,
        replacements: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut writer = ZipWriter::new(File::create(path)?);
        writer.set_raw_comment(self.archive.comment().into());
        for i in 0..self.archive.len() {
            super::check(cancel)?;
            let file = self.archive.by_index(i)?;
            if let Some(replacement) = replacements.get(file.name()) {
                xml::parse(replacement, cancel)?;
                let mut options =
                    SimpleFileOptions::default().compression_method(file.compression());
                if let Some(time) = file.last_modified() {
                    options = options.last_modified_time(time);
                }
                if let Some(mode) = file.unix_mode() {
                    options = options.unix_permissions(mode);
                }
                writer.start_file(file.name(), options)?;
                writer.write_all(replacement.as_bytes())?;
            } else {
                writer.raw_copy_file(file)?;
            }
        }
        writer.finish()?.sync_all()?;
        Ok(())
    }
}
