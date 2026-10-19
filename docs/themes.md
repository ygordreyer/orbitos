# Native themes

OrbitOS renders native theme descriptors in software. Themes are loaded from `Themes/<id>/theme.json` on the SD card, with assets inside the same theme directory.

## Included themes

| Name | Directory ID | Options |
| --- | --- | --- |
| Orbit Hero | `orbit-hero` | Square or Tall covers |
| Orbit Row | `orbit-row` | Square or Tall covers |
| Big Picture | `big-picture` | Square or Tall covers |
| Compact+ | `compact-plus` | Square or Tall covers; orange, Big Picture, white or black color scheme |

Choose a theme and its options in Settings, Appearance. These descriptors cover Library, Recents, Search, Apps, Settings, game Details, keyboard, context and in-game menus, disc selection, guides, notifications and volume/brightness overlays.

## Selection files

The current selection is `.allium/state/ui-theme.json`. Its schema stores `schema_version`, `id`, `revision`, descriptor `digest`, `generation`, current `settings` and saved `theme_settings` by theme ID. Prefer the picker to editing this generated file. The native selection does not rewrite the old `.allium/state/stylesheet.json`.

Repository `themes/choices.json` is an inventory of the exported descriptors and their byte counts. The device discovers theme directories and validates `theme.json`; it does not use this inventory as its picker configuration. The picker accepts only the four IDs in `LAUNCHER_PACKS` in `crates/common/src/theme/choices.rs`.

Descriptors, assets and fonts are validated before activation. Missing or invalid packs can produce a diagnostic and fallback presentation. Installing a launcher binary alone does not install external theme descriptors.

## Add or modify a theme

1. Start from a descriptor under repository `themes/`. Keep its directory name and descriptor `id` equal, and give changed content an appropriate `revision`.
2. Keep assets within the theme directory. Use font IDs supported by the font bank. Define the required screens and bindings in the descriptor schema implemented by `crates/common/src/theme/pack.rs`. The manifest limit is 131072 bytes.
3. Check loading and rendering with the host tests and simulator described in [development](development.md). Test every declared option, including both cover shapes.
4. Copy the pack directory into SD `Themes/`, or use `make dist` to copy repository themes into the package.

A modified pack using one of the four existing IDs can be selected with the current picker. An additional ID needs a source change to `LAUNCHER_PACKS` and a rebuilt launcher before it appears there. Adding a row to `themes/choices.json` alone does not add a picker entry.

The optional web descriptor exporter is documented in [development](development.md). A web project is not required to build or use the checked-in native themes. Artwork lookup and fitting are documented in [media](media.md).
