//! Release metadata is pinned in the application, never supplied by a workspace
//! or an install request. Catalog refreshes require a reviewed application update.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct License {
    pub identifier: String,
    pub url: String,
    pub notice: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub revision: String,
    pub project_url: String,
    pub license: License,
    #[serde(default)]
    pub requires: Vec<String>,
    pub variants: Vec<Variant>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Variant {
    pub platform: String,
    pub files: Vec<Download>,
    #[serde(default)]
    pub exports: BTreeMap<String, Export>,
    #[serde(default)]
    pub source_build: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Download {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub format: Format,
    pub destination: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Raw,
    Gzip,
    TarGz,
    TarBz2,
    Zip,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Export {
    pub basename: String,
    #[serde(default)]
    pub executable: bool,
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub fn catalog() -> Result<Vec<Asset>> {
    let assets: Vec<Asset> = serde_json::from_str(include_str!("catalog.json"))?;
    validate(&assets)?;
    Ok(assets)
}

pub(super) fn validate(assets: &[Asset]) -> Result<()> {
    ensure!(assets.len() <= 64, "Asset catalog exceeds limit");
    let mut ids = HashSet::new();
    for asset in assets {
        ensure!(
            safe_name(&asset.id) && ids.insert(asset.id.clone()),
            "Invalid/duplicate asset ID"
        );
        ensure!(
            !asset.license.identifier.is_empty() && !asset.license.url.is_empty(),
            "Asset license metadata is required"
        );
        let mut platforms = HashSet::new();
        for variant in &asset.variants {
            ensure!(
                platforms.insert(&variant.platform) && variant.files.len() <= 16,
                "Invalid asset variant"
            );
            for file in &variant.files {
                ensure!(
                    file.sha256.len() == 64 && file.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
                    "Invalid pinned SHA256"
                );
                ensure!(
                    file.size_bytes > 0 && file.size_bytes <= 16 * 1024 * 1024 * 1024,
                    "Asset exceeds individual 16GiB budget"
                );
                ensure!(
                    safe_relative(&file.destination),
                    "Invalid asset destination"
                );
                let url = url::Url::parse(&file.url)?;
                ensure!(
                    url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "Catalog assets require credential-free HTTPS"
                );
            }
            for export in variant.exports.values() {
                ensure!(safe_name(&export.basename), "Invalid exported file name");
            }
        }
    }
    for asset in assets {
        for id in &asset.requires {
            ensure!(ids.contains(id), "Unknown asset dependency: {id}");
        }
    }
    Ok(())
}

impl Asset {
    pub fn variant(&self, platform: &str) -> Result<&Variant> {
        self.variants
            .iter()
            .find(|variant| variant.platform == platform)
            .or_else(|| {
                self.variants
                    .iter()
                    .find(|variant| variant.platform == "any")
            })
            .with_context(|| format!("{} has no verified package for {platform}", self.name))
    }
}

pub(super) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
pub(super) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && std::path::Path::new(path)
            .components()
            .all(|p| matches!(p, std::path::Component::Normal(_)))
}
