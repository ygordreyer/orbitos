# OrbitOS

OrbitOS is a fork of [Allium by goweiwen (Wei Wen Goh) and contributors](https://github.com/goweiwen/Allium), based on commit `316d9eb`. The fork repository is [ygordreyer/orbitos](https://github.com/ygordreyer/orbitos).

This is an alpha, named **v0.1.0-alpha**. Only the **Miyoo Mini Plus** is verified. Other devices supported by Allium, including the Miyoo Mini and Miyoo Mini Flip, are untested with OrbitOS.

The binaries still use the inherited package version `1.0.2`, and About still displays the channel `stable`. Those strings do not describe this fork's release maturity.

## Features

### Themes and interface

- Four native themes: Orbit Hero, Orbit Row, Big Picture and Compact+. Each supports Square and Tall covers. Compact+ also has four color schemes.
- Themed Library, Recents, Search, Apps, Settings, keyboard, game Details, context menus, guide reader and volume/brightness overlays.
- Saved theme choices and per-theme options, with descriptor validation and fallback handling.
- START opens game Details with available metadata, descriptions and play history.

### Library and media

- Favorites, Recents and play activity inherited from Allium.
- Unsupported files and CUE/M3U members are hidden from game lists. Folders with one launchable game can appear as a single game.
- Duplicate display names gain distinguishing filename tags. Search merges local and configured network games, removes folder/member aliases and ranks title matches before developer/publisher matches.
- Read-only media lookup supports square/tall covers, logos, backgrounds, screenshots, gamelist metadata and legacy artwork paths. Artwork loads in background work and uses memory and disk caches.

### In-game menu

- Themed presentation of Allium's continue, save, load, reset, guide, settings and quit actions.
- Guide pagination and Unicode cursor/search fixes, clean paused-game snapshots and volume/brightness overlays.
- M3U startup disc selection for up to eight discs, restricted to PCSX ReARMed and the required shared-save configuration.

### Network library

- An optional, manually configured PlayStation catalog with persistent cards and cache status.
- Cancellable downloads, HTTP Range resume, SHA-256 verification at completion and a sampled-fingerprint cached-launch fast path.
- Explicit verification/adoption of existing files and manual unload through Manage storage. There is no automatic eviction.

### Rendering and system handling

- Software rendering with retained frames, cached text/artwork, idle preparation of adjacent Recents and Details frames, and framebuffer damage updates.
- Background battery/Wi-Fi status sampling, batched database work and separate frontend/game CPU governor settings with input-triggered boosts.
- Existing Allium Wi-Fi, file transfer, Syncthing, language, clock, display and power controls remain available.

See [themes](docs/themes.md), [media](docs/media.md), [network library](docs/network-library.md) and the [changelog](CHANGELOG.md) for details. No measured FPS, latency or battery-life improvement is claimed here.

## Installation

A prebuilt OrbitOS package is not published yet. The [`v0.1.0-alpha` release](https://github.com/ygordreyer/orbitos/releases) carries the source only. Build the package as described in [development](docs/development.md). An upstream Allium archive installs upstream Allium, not OrbitOS.

### Fresh SD card

1. Format the card as FAT32.
2. Extract the complete package into the card root, including hidden directories. It contains `.allium`, `.tmp_update`, `Apps`, `RetroArch`, `Themes`, `BIOS`, `Roms` and `Saves`.
3. Add your own games and any required BIOS files. The package does not supply a game library.
4. Eject the card safely, insert it into the Miyoo Mini Plus and boot. Keep power connected while first-boot migrations run.

### Over an existing Allium installation

1. Power off the device and back up the card. Preserve the previous `.allium`, `.tmp_update`, `Apps`, `RetroArch`, `Themes` and `Saves`, including `Saves/CurrentProfile/allium.db`.
2. Extract the OrbitOS package elsewhere. Copy `.allium`, `.tmp_update`, `Apps`, `RetroArch` and `Themes` to the card root, replacing matching files. Include hidden directories. Preserve your games, BIOS files and saves.
3. Eject safely and boot. Boot scripts and the database can apply migrations.

### Upgrade OrbitOS

Back up the same directories before each upgrade, then overlay the five software directories listed above from the new OrbitOS package. Keep the backup until the new installation has been checked on the device.

System Update is **manual-only**. The retained update backend still points at upstream Allium releases, not OrbitOS. It is not an OrbitOS update channel. The boot updater also retains support for a manually supplied `allium-ota.zip`.

### Roll back

Rollback is manual. There is no rollback target or packaged rollback tool. With the device powered off, restore the previous `.allium` and `.tmp_update`, plus the matching backed-up `Apps`, `RetroArch` and `Themes`. Restore the pre-upgrade database/state and any other replaced files from the backup. Replacing only the launcher does not undo database migrations. Compatibility of an upgraded database with older binaries is not established.

## Build and development

The device build uses Rust nightly, cargo-zigbuild, Zig and a Docker toolchain. `make all` assembles `dist/`; it does not create a ZIP. See [development](docs/development.md) for requirements, host tests, simulator use, deployment scope and packaging commands.

## Acknowledgements

Allium is only possible thanks to the Miyoo Mini community, including but not limited to:
- eggs: RetroArch port, [many code samples](https://www.dropbox.com/sh/hqcsr1h1d7f8nr3/AABtSOygIX_e4mio3rkLetWTa), answering questions on Discord
- [Onion team](https://github.com/OnionUI/Onion) (Aemiii91, Schmurtz, Totofaki, and more): Maintaining a sane-defaults RetroArch configuration, and the huge village
- kebabstorm: [Miyoo Mini resources](https://github.com/anzz1/miyoomini-resources)
- shauninman: Allium is heavily inspired by [MiniUI](https://github.com/shauninman/MiniUI)'s simplicity and clean design
- [steward-fu](https://github.com/steward-fu): miraculous DraStic port
- Early adopters and testers of Allium
- [Icons8.com](https://icons8.com) for the icons used in the wiki.

## License

OrbitOS retains Allium's [MIT license](LICENSE), including the copyright notice for Wei Wen Goh. Bundled fonts and third-party components retain their own license files.
