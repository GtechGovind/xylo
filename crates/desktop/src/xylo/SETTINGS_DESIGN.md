# Settings design system

Every settings destination is built from the primitives in `settings_ui.rs`.
Pages do not set their own paddings, type sizes or borders; if a page needs a
pattern that is missing here, add a primitive and document it below.

## Layout

The sidebar is `settings_sidebar` with `settings_nav_item` rows, built in
`Xylo::settings_navigation` (settings.rs); add new destinations there.

| Part | Measure | Constant |
| --- | --- | --- |
| Settings sidebar width | 220 px | `SIDEBAR_WIDTH` |
| Sidebar row | 32 px, 7 px radius, active row filled with `selected()` | `NAV_ROW_HEIGHT` |
| Sidebar group gap | 16 px; group header 28 px, 12 px muted | |
| Content column | left-aligned, max 680 px | `CONTENT_MAX_WIDTH` |
| Content gutter | 48 px left and right | `CONTENT_GUTTER` |
| Space above the header | 32 px | `CONTENT_TOP` |
| Space between sections | 32 px (also header to first section) | `SECTION_GAP` |
| Setting row | min 56 px, 13 px vertical padding, 24 px text-to-control gap | `ROW_MIN_HEIGHT`, `ROW_PADDING_Y` |
| List item | fixed 56 px, 14 px side padding | `LIST_ITEM_HEIGHT`, `LIST_PADDING_X` |
| Monogram | 28 px tile, 7 px radius | `MONOGRAM_SIZE` |
| Controls | 32 px high, 8 px radius | `CONTROL_HEIGHT` |
| Bordered containers | 10 px radius, `border()` hairline, `panel()` fill | `CONTAINER_RADIUS` |

Spacing steps are 2, 4, 6, 8, 12, 16, 24, 32 and 48 px. List items are fixed
height so long lists can use `uniform_list` at `LIST_ITEM_HEIGHT` inside
`list_frame()`, with `ListItem::divider(index > 0)`.

## Type

| Role | Size | Weight | Colour |
| --- | --- | --- | --- |
| Page title | 22 px | semibold | `text()` |
| Page description | 13.5 px | regular | `muted()` |
| Section title | 14 px | semibold | `text()` |
| Row and item label | 14 px | medium | `text()` |
| Description | 13 px, at most 2 lines in rows | regular | `muted()` |
| Caption, pill, breadcrumb, group header | 12 px | medium or regular | `muted()` |
| Tag chip | 11 px | medium | `text_secondary()` on `well()` |

Any text that can be clipped uses `.single_line()` from `crate::ui` inside a
row that centres its items. Text inside buttons is a single-line child, never
the toolkit button label.

## Page anatomy

```
settings_page(id, "Providers")            breadcrumb (sub-pages only)
  .description("…")                       title            [actions]
  .breadcrumb(breadcrumb([crumb(..)], ..)) description
  .action(primary_button(..))             ── 32 px ──
  .child(section(title, desc, rows([..])))  section …
  .child(danger_zone([..]))                 Danger zone (last)
```

Sub-pages show a breadcrumb ("Providers / ChatGPT") above the title; there
are no floating back buttons.

## Rows

```
| Label (14, medium)                          [control] |
| Description (13, muted, ≤ 2 lines)                    |
| ✓ Saved / ⚠ error text (feedback, optional)           |
```

`setting_row(label, description, control)`; rows inside a section go through
`rows([...])`, which draws a hairline between neighbours only. Rows have no
side padding, so they align with the section title; `.inset()` pads them for
use inside a bordered container. Inline outcomes use `.feedback(Feedback::…)`;
toasts are for global events only.

## Lists

`list_container([...])` holds grouped items (providers, environments,
connectors, plugins, approval rules). An item is
`list_item(id, Some(monogram(name, stable_id)), title, subtitle)` with
`.trailing(status_pill(..))`, actions, `.tags(..)` for kinds and
`.hover_detail(raw_id)`. `.on_click` turns the leading part into the button
that opens the item and adds a chevron; trailing controls stay outside it.
Monograms take initials from the name and a tint from the stable id, so
renaming keeps the colour; never use brand logos. `skeleton_list_item` stands
in while the first load runs.

Status pills: Connected and Ready (success), Needs setup (warning), Update
available (accent), Disabled (neutral), Error (danger).
`pill(label, tone)` covers custom wording.

## Controls

- Booleans: `switch(id, row_label, checked)` (an `XyloSwitch` with a hidden
  label); the row label is its name.
- Enums: `select(id, label, options)` (compact dropdown, right-aligned).
  Two or three short options, and view switches ("Discover · Installed ·
  Sources"), use `segmented(id, options)`; `SelectOption::id` keeps an
  addressable id per segment.
- Search: `search_field(state, label)`, filtering as the person types.
- Read-only values on the right of a row: `value_text`.
- Presets with consequences: `radio_list(id, [radio_option(..)])`, with
  `.caution(true)` on risky presets.
- Buttons, 32 px: `primary_button` (the one main action per page),
  `secondary_button` (outlined), `text_button` (quiet, in rows),
  `destructive_button` (red text). Secondary row actions go in
  `overflow_menu`.
- Forms: `form_column([field(label, input, help), …,
  form_actions([...])])`; labels sit above full-width inputs; a failed save
  shows `inline_error` above the form.

## Loading, feedback and motion

- Lists that load show `loading_list(rows, started_at)`: skeleton items, then
  "Taking longer than usual…" after 3 s. Call `self.arm_slow_hint(cx)` when
  the request is sent and keep the returned `Instant` in the page state;
  `slow_caption(started_at)` covers other waits.
- Every action that waits on the host is `busy(button, busy)`: disabled with a
  spinner, same width and accessible name. Determinate work uses
  `crate::motion::progress_bar`.
- Row-level outcomes use `SettingRow::feedback`; page-level failures use
  `inline_error`. Toasts are for global events only.
- Motion comes from `crate::motion` and respects reduced motion: pages rise
  in, sub-pages (with a breadcrumb) slide in, list items fade in. All of it is
  built into the primitives; pages add only `fade_on_change` for status text
  replaced in place and `collapse` for disclosures.

## Empty states and destructive actions

`empty_state(icon, title, description, Some(primary_button(..)))`: one line
each, one action. Irreversible actions live in `danger_zone` at the end of the
page; their handler confirms with `Xylo::confirm_destructive` (Cancel first)
before acting.

## Icons

Icons name a concept through `crate::icons::AppIcon` and use its sizes:
`icons::BASE` (16 px) for navigation and rows, `icons::SMALL` (14 px) for
inline marks, carets and chevrons. No other icon sizes.

## Themes

All colours come from `crate::ui` tokens (`bg`, `panel`, `well`, `border`,
`border_strong`, `text`, `text_secondary`, `muted`, `faint`, `selected`,
`hover_panel`, and the status pairs), so light and dark follow the shared
palette. Monogram tints are the only local colours; a test keeps each at
4.5:1 contrast in both themes.
