//! Xylo's icon system: one soft glyph per concept, two sizes, quiet colour.
//!
//! Every icon is a Lucide outline drawn with a 1.5 stroke (`crate::assets`
//! softens the whole catalog, toolkit components included), so weight is
//! uniform by construction. This module makes the remaining choices uniform:
//! which glyph a concept uses, how large it is, and how it is coloured.
//!
//! # Usage rules
//! - Name icons by meaning with [`AppIcon`], never by glyph. If a concept is
//!   missing, add a variant here (and the glyph to `ExtraIcons` in
//!   `crate::assets` when it is not a toolkit default) instead of reaching for
//!   `Glyph` at the call site. One concept maps to one glyph, and no two
//!   concepts share a glyph; the tests enforce both.
//! - Two sizes only: [`BASE`] (16 px) for toolbar, navigation and row icons;
//!   [`SMALL`] (14 px) for inline metadata, status marks, chips and inline
//!   spinners. Do not pass other pixel sizes. [`LARGE`] is reserved for a
//!   full-page spinner; brand marks and illustrations are not icons.
//! - Colour comes from the container: icons inherit the text colour, so a
//!   control sets [`rest_color`] at rest and [`hover_color`] on hover and the
//!   glyph follows. Status icons use [`status`], which applies the tone.
//! - Icon-only controls use [`icon_button`]: a 28 px hit area, a soft rounded
//!   hover fill, and a tooltip that doubles as the accessible name. Icon-only
//!   dropdown triggers use [`menu_trigger`], which matches that box.
//!
//! # Mapping
//! | Concept | Variant | Glyph |
//! |---|---|---|
//! | New chat | `NewChat` | SquarePen |
//! | Search | `Search` | Search |
//! | Add (generic) | `Add` | Plus |
//! | Open or add a project | `OpenProject` | FolderPlus |
//! | Settings | `Settings` | Settings |
//! | More actions | `More` | Ellipsis |
//! | Close, dismiss, remove chip | `Close` | X |
//! | Back | `Back` | ArrowLeft |
//! | Edit, rename | `Edit` | Pencil |
//! | Delete | `Delete` | Trash |
//! | Refresh a list | `Refresh` | RotateCw |
//! | Retry a failed turn or step | `Retry` | RotateCcw |
//! | Copy | `Copy` | Copy |
//! | Read aloud | `ReadAloud` | Volume2 |
//! | Voice input, voice settings | `Mic` | Mic |
//! | Send message | `Send` | ArrowUp |
//! | Stop a running turn | `Stop` | Square |
//! | Terminal | `Terminal` | Terminal |
//! | Files panel | `Files` | Files |
//! | Changes (diff) panel | `Changes` | FileDiff |
//! | Git branch | `Branch` | GitBranch |
//! | Agent, subagents | `Agent` | Bot |
//! | Pinned chat | `Pin` | Pin |
//! | Saved drafts | `Drafts` | NotebookPen |
//! | Scheduled tasks | `Scheduled` | CalendarClock |
//! | Split a pane | `Split` | Columns2 |
//! | Toggle sidebar | `SidebarToggle` | PanelLeft |
//! | Toggle side panel | `PanelToggle` | PanelRight |
//! | Expanded / open menu | `ChevronDown` | ChevronDown |
//! | Collapsed / navigate into | `ChevronRight` | ChevronRight |
//! | Collapse upward | `ChevronUp` | ChevronUp |
//! | Select (picker caret) | `Select` | ChevronsUpDown |
//! | Success | `Success` | CircleCheck |
//! | Warning | `Warning` | TriangleAlert |
//! | Error | `Error` | CircleAlert |
//! | Information | `Info` | Info |
//! | Done, selected | `Done` | Check |
//! | Pending step | `Pending` | CircleDashed |
//! | Loading (spin it) | `Loading` | LoaderCircle |
//! | Folder | `Folder` | Folder |
//! | Expanded folder | `FolderOpen` | FolderOpen |
//! | File | `File` | File |
//! | Image | `Image` | Image |
//! | Web | `Web` | Globe |
//! | Memory | `Memory` | Brain |
//! | Model, provider | `Model` | Sparkles |
//! | Reasoning effort | `Reasoning` | Gauge |
//! | Skills | `Skills` | Lightbulb |
//! | Plan, to-do list | `Plan` | ListChecks |
//! | Read tool | `ToolRead` | FileSearch |
//! | Edit tool | `ToolEdit` | FilePen |
//! | Other tools | `Tool` | Wrench |
//! | Notifications | `Notifications` | Bell |
//! | Chat settings | `Chat` | MessageSquare |
//! | General settings | `General` | SlidersHorizontal |
//! | Connectors | `Connectors` | Plug |
//! | Plugins | `Plugins` | Puzzle |
//! | Environments, remote server | `Environment` | Server |
//! | Container (Docker, Podman) | `Container` | Container |
//! | Storage, downloads location | `Storage` | HardDrive |
//! | Empty list | `Empty` | Inbox |
//! | Permissions (settings page) | `Permissions` | Shield |
//! | Permission: ask first | `PermissionAsk` | ShieldQuestionMark |
//! | Permission: approve for me | `PermissionAuto` | ShieldCheck |
//! | Permission: full access | `PermissionFull` | ShieldAlert |
//! | Permission: read only | `PermissionReadOnly` | Eye |
use crate::ui::{self, XyloTooltip, focus_ring};
use gpui_kit::assets::IconName as Glyph;
use gpui_kit::component::button::{Button as MenuButton, ButtonVariants};
use gpui_kit::component::{Icon, IconNamed};
use gpui_kit::{prelude::*, *};

/// Inline metadata, status marks and chips.
pub const SMALL: Pixels = px(14.);
/// Toolbar, navigation and row icons.
pub const BASE: Pixels = px(16.);
/// Full-page progress (a spinner centred on an otherwise empty page) only.
pub const LARGE: Pixels = px(20.);
/// Square hit area of every icon-only control.
pub const HIT_AREA: Pixels = px(28.);
/// Corner radius of the hover fill behind an icon-only control.
pub const HIT_RADIUS: Pixels = px(6.);

/// Icon colour at rest: present but quiet.
pub fn rest_color() -> u32 {
    ui::muted()
}
/// Icon colour on hover, focus or when its control is active.
pub fn hover_color() -> u32 {
    ui::text()
}

macro_rules! app_icons {
    ($($(#[doc = $doc:literal])+ $name:ident => $glyph:ident,)+) => {
        /// A concept that has an icon. See the module doc for the mapping.
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        pub enum AppIcon {
            $($(#[doc = $doc])+ $name,)+
        }

        impl AppIcon {
            /// Every concept, in declaration order.
            #[cfg(test)]
            pub const ALL: &'static [Self] = &[$(Self::$name,)+];

            /// The one glyph this concept is drawn with.
            pub const fn glyph(self) -> Glyph {
                match self {
                    $(Self::$name => Glyph::$glyph,)+
                }
            }
        }
    };
}

app_icons! {
    /// Start a new chat.
    NewChat => SquarePen,
    /// Search conversations, files or settings.
    Search => Search,
    /// Add an item to a list.
    Add => Plus,
    /// Open or add a project folder.
    OpenProject => FolderPlus,
    /// Open settings.
    Settings => Settings,
    /// Overflow menu of further actions.
    More => Ellipsis,
    /// Close, dismiss or remove.
    Close => X,
    /// Return to the previous screen.
    Back => ArrowLeft,
    /// Edit or rename.
    Edit => Pencil,
    /// Delete.
    Delete => Trash,
    /// Reload a list or view.
    Refresh => RotateCw,
    /// Retry a failed turn or step.
    Retry => RotateCcw,
    /// Copy to the clipboard.
    Copy => Copy,
    /// Read a message aloud.
    ReadAloud => Volume2,
    /// Voice input and voice settings.
    Mic => Mic,
    /// Send the composed message.
    Send => ArrowUp,
    /// Stop the running turn.
    Stop => Square,
    /// Terminal.
    Terminal => Terminal,
    /// Project files panel.
    Files => Files,
    /// Changes (diff) panel.
    Changes => FileDiff,
    /// Git branch.
    Branch => GitBranch,
    /// The agent and its subagents.
    Agent => Bot,
    /// Pinned chat.
    Pin => Pin,
    /// Saved drafts.
    Drafts => NotebookPen,
    /// Scheduled tasks.
    Scheduled => CalendarClock,
    /// Split a pane in two.
    Split => Columns2,
    /// Show or hide the sidebar.
    SidebarToggle => PanelLeft,
    /// Show or hide the side panel.
    PanelToggle => PanelRight,
    /// Expanded section or open menu.
    ChevronDown => ChevronDown,
    /// Collapsed section or navigate into.
    ChevronRight => ChevronRight,
    /// Collapse upward.
    ChevronUp => ChevronUp,
    /// Picker caret.
    Select => ChevronsUpDown,
    /// Success.
    Success => CircleCheck,
    /// Warning.
    Warning => TriangleAlert,
    /// Error or failure.
    Error => CircleAlert,
    /// Neutral information.
    Info => Info,
    /// Done or selected.
    Done => Check,
    /// A step that has not started.
    Pending => CircleDashed,
    /// Work in progress; spin it with `crate::motion`.
    Loading => LoaderCircle,
    /// Folder.
    Folder => Folder,
    /// Expanded folder.
    FolderOpen => FolderOpen,
    /// File.
    File => File,
    /// Image or image generation.
    Image => Image,
    /// Web access, fetch and search.
    Web => Globe,
    /// Memory.
    Memory => Brain,
    /// Model or model provider.
    Model => Sparkles,
    /// Reasoning effort.
    Reasoning => Gauge,
    /// Skills.
    Skills => Lightbulb,
    /// Plan or to-do list.
    Plan => ListChecks,
    /// A tool that reads files.
    ToolRead => FileSearch,
    /// A tool that edits files.
    ToolEdit => FilePen,
    /// Any other tool.
    Tool => Wrench,
    /// Notifications.
    Notifications => Bell,
    /// Chat settings.
    Chat => MessageSquare,
    /// General settings.
    General => SlidersHorizontal,
    /// Connectors.
    Connectors => Plug,
    /// Plugins.
    Plugins => Puzzle,
    /// Environments and remote servers.
    Environment => Server,
    /// Docker or Podman container.
    Container => Container,
    /// Storage and download locations.
    Storage => HardDrive,
    /// Empty list.
    Empty => Inbox,
    /// Permissions as a whole (the settings page).
    Permissions => Shield,
    /// Permission preset: ask before acting.
    PermissionAsk => ShieldQuestionMark,
    /// Permission preset: approve routine actions automatically.
    PermissionAuto => ShieldCheck,
    /// Permission preset: full access.
    PermissionFull => ShieldAlert,
    /// Permission preset: read only.
    PermissionReadOnly => Eye,
}

impl AppIcon {
    /// Semantic colour for status concepts; `None` means inherit.
    pub fn tone(self) -> Option<u32> {
        match self {
            Self::Success => Some(ui::success()),
            Self::Warning => Some(ui::warning()),
            Self::Error => Some(ui::danger()),
            Self::Info => Some(rest_color()),
            _ => None,
        }
    }
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        self.glyph().path()
    }
}

/// A [`BASE`] icon that inherits its container's text colour.
pub fn icon(name: AppIcon) -> Icon {
    Icon::new(name).size(BASE)
}
/// A [`SMALL`] icon that inherits its container's text colour.
pub fn small(name: AppIcon) -> Icon {
    Icon::new(name).size(SMALL)
}
/// A [`SMALL`] status mark in its semantic tone (see [`AppIcon::tone`]).
pub fn status(name: AppIcon) -> Icon {
    let icon = small(name);
    match name.tone() {
        Some(tone) => icon.text_color(rgb(tone)),
        None => icon,
    }
}

/// Icon-only control: a 28 px hit area with a soft rounded hover fill, muted
/// at rest and primary on hover. `tooltip` is also the accessible name.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: AppIcon,
    tooltip: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    button_frame(id, tooltip).child(icon(name))
}

/// Icon-only trigger for a toolkit dropdown menu ("…", "+"), sized like
/// [`icon_button`]: the toolkit draws a medium icon (16 px, [`BASE`]) and the
/// instance size below overrides its 32 px default box. `label` is the
/// accessible name.
pub fn menu_trigger(
    id: impl Into<ElementId>,
    name: AppIcon,
    label: impl Into<SharedString>,
) -> MenuButton {
    MenuButton::new(id)
        .ghost()
        .icon(name)
        .accessibility_label(label)
        .size(HIT_AREA)
        .rounded(HIT_RADIUS)
}

/// The [`icon_button`] frame without its glyph, for the rare control whose
/// glyph is not an [`AppIcon`] (a brand mark, a spinner).
pub fn button_frame(
    id: impl Into<ElementId>,
    tooltip: impl Into<SharedString>,
) -> gpui_kit::base::Button {
    let tooltip: SharedString = tooltip.into();
    focus_ring(gpui_kit::base::Button::new(id))
        .accessibility_label(tooltip.clone())
        .tooltip(move |_, cx| cx.new(|_| XyloTooltip(tooltip.clone())).into())
        .size(HIT_AREA)
        .flex_shrink_0()
        .rounded(HIT_RADIUS)
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(rest_color()))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ui::hover())).text_color(rgb(hover_color())))
        .active(|style| style.bg(rgb(ui::pressed())))
}

#[cfg(test)]
#[path = "icons/tests.rs"]
mod tests;
