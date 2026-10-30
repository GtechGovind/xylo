//! Application asset source: the toolkit's default icons plus the extra
//! glyphs Xylo's design uses. Only the listed SVGs are embedded.
use gpui_kit::AssetSource;
use gpui_kit::assets::icon_assets;

icon_assets!(
    ExtraIcons,
    [
        FolderOpen,
        SquarePen,
        Sparkles,
        MessageSquare,
        PencilLine,
        Paperclip,
        Image,
        SlidersHorizontal,
        Brain,
        Wrench,
        Terminal,
        GitBranch,
        FilePen,
        FileSearch,
        Globe,
        Hammer,
        ListChecks,
        Check,
        LoaderCircle,
        CircleDashed,
        ChevronsUpDown,
        Lightbulb,
        ShieldCheck,
        Eye,
        Clock,
        X,
        Send,
    ]
);

/// Xylo's own mark: a prompt chevron and cursor line (the coding side) with a
/// four-point spark (the assistant side). Drawn for this product.
use crate::ui::XYLO_MARK;
const XYLO_MARK_SVG: &[u8] = include_bytes!("../assets/xylo-mark.svg");

#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if path == XYLO_MARK {
            return Ok(Some(std::borrow::Cow::Borrowed(XYLO_MARK_SVG)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if XYLO_MARK.starts_with(path) {
            paths.push(XYLO_MARK.into());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
