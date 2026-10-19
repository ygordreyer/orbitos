# Changelog

## v0.1.0-alpha (2026-10-19)

First OrbitOS alpha, based on Allium `316d9eb` by goweiwen and contributors. Verified only on Miyoo Mini Plus. Package version and About channel strings remain inherited as `1.0.2` and `stable`.

### Themes and UI

- Added Orbit Hero, Orbit Row, Big Picture and Compact+ native descriptors, Square/Tall cover options and Compact+ color schemes.
- Applied native rendering to Library, Recents, Search, Apps, Settings, keyboard, Details, dialogs, guide reader and volume/brightness overlays.
- Added persistent theme selection/options, descriptor validation, previews and fallback handling.
- Added OrbitOS identity, upstream attribution, build identification, About information and boot splash.

### Library

- Added game Details with imported metadata, descriptions and play history, plus shared gamelist and media lookup.
- Hid unsupported files and CUE/M3U members, collapsed folders containing one launchable game and retained folder launch identity.
- Disambiguated repeated display names using filename tags, preserved dotted folder tags and deduplicated folder/member search results.
- Ranked title search matches ahead of developer and publisher matches before applying the result limit.
- Added background metadata refresh and artwork resolution/decoding, with square/tall cover, logo and background support.

### In-game menu

- Restyled existing actions and guide screens with native theme rows, artwork and status.
- Added M3U startup disc selection for up to eight discs under PCSX ReARMed and the checked shared-save configuration.
- Changed menu opening to discard stale input and retain a clean paused-game snapshot.
- Added themed volume/brightness overlay handling over emulator frames.

### Network library

- Added a manually provisioned PlayStation catalog for private LAN or loopback HTTP endpoints, with library/search cards and cache status.
- Added cancellable resumable downloads, exact HTTP response validation, full SHA-256 verification at completion and a sampled-fingerprint cached-launch fast path.
- Added explicit adoption, allocation-aware admission reserves, manual storage management and interrupted-unload recovery. No automatic eviction is enabled.

### Performance

- Added retained rendering, text/artwork caches, framebuffer damage handling, packed pixel rows and ARM NEON pixel/raster kernels.
- Prepared neighboring Recents frames and Details artwork/text during idle work, with bounded frame reuse.
- Moved status sampling off UI threads, batched metadata queries and play updates, and reduced database trigger work.
- Added frontend/game governor parameters, input-triggered frequency boosts and release of inactive raster buffers with allocator maintenance.
- These changes describe mechanisms; no measured speed or battery-life gain is asserted.

### Fixes

- Fixed Unicode guide cursor/search handling and bounded incremental pagination/history.
- Corrected timezone offset labels and clock refresh after changes.
- Corrected cover fitting, logo fallback/suppression and long-title layouts.
- Corrected folder/member launch and display identity handling, including network cache guards before launch side effects.
- Changed System Update to manual-only information; the retained backend still targets upstream Allium.

### Packaging

- Included repository native themes in distribution assembly and clarified the limited directory scope of `deploy-all`.
- Replaced commercial artwork fixtures with reproducible synthetic PNGs and removed personal-path and untracked-theme dependencies from the affected tests/tooling.
- Replaced early feature notes with current installation, themes, media, network and development documentation.
