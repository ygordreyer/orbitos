# Opt-in native Orbit library candidate

This local candidate changes the existing Rust Games presentation. The original Allium checkout remains at the official v1.0.2 commit `316d9eb9d43737f5ee0f8e6e449d471a09260a28`. This development worktree is on a local, unpublished branch. No device/card deployment or upstream submission has occurred.

## Enable and roll back

At launcher startup, either `ALLIUM_ORBIT=1` or a regular `.allium/state/orbit-enabled` file enables the candidate. The opt-in is read once per launcher process. This code never creates that marker. Removing the marker, unsetting the environment flag, and starting the official launcher restores the default presentation. A later SSH-shell environment export does not change an already running daemon's environment; use the marker only during a separately authorized device deployment.

The candidate targets the Mini Plus 640 × 480 frame. Games owns its 52-to-438-pixel body, with a 210-pixel hero, five visible 101-pixel cover cards at 118-pixel pitch, cyan selection and two caption lines. App owns the 52-pixel header and 42-pixel footer, and keeps the existing live clock/battery status. A long title wraps within the left 250-pixel text pane, up to three lines.

This first candidate is static. Selection snaps immediately; carousel animation, a 33.3-millisecond animation timer, async image prefetch and timing instrumentation are intentionally deferred. The launcher event loop and idle interval are unchanged. There is no browser runtime, video, blur, 3D shader, invented progress database, rating screen or guide page.

## Input and existing behavior

Left/Right and Up/Down move the same existing logical selection. A follows `EntryList::select_entry`, including directory navigation and `ConsoleMapper::launch_game`; B returns through the existing CloseView path. Context-menu launch/reset/core/favorite behavior, Y sorting, X search, persistence, daemon lifecycle and resume mechanisms remain the existing Allium paths. L/R switch tabs when the active view does not consume them. Settings and context menus retain their own shoulder paging ownership. Recents, Apps and Settings retain their existing presentation.

This preserves routing; it does not prove a particular game's native launch, quit-return, power resume or migrated save compatibility.

## Optional artwork contract

Stock covers remain unchanged at `Roms/PS/Imgs/<outer filename stem>.png`. For example, an outer `R4 - Ridge Racer Type 4 (USA).m3u` folder uses:

- `Imgs/R4 - Ridge Racer Type 4 (USA).hero.png`: exactly 360 × 210, host-cropped screenshot.
- `Imgs/R4 - Ridge Racer Type 4 (USA).orbit.png`: exactly 101 × 101, white canvas with the source cover fitted inside 93 × 93 at a four-pixel margin and optional nine-pixel rounded alpha.

The renderer looks in the entry's immediate parent `Imgs` folder. It does not implement the stock cover lookup's upward recursive search for optional Orbit derivatives. A missing, oversized, invalid or incorrectly sized derivative gets a placeholder, and the failed lookup is cached. The stock art file is never altered or used as an unbounded runtime scaling input.

Only the active leaf EntryList retains Orbit artwork. Descent clears each ancestor cache; return prepares that parent again. Loading a persisted nested path prepares only its leaf. The Orbit LRU across that active chain therefore has at most 16 positive/negative entries. Even if all entries were 360 × 210 RGBA heroes, decoded pixel storage is at most 4,838,400 bytes. A read is capped at 512 KiB plus one byte and dimensions are checked before PNG decoding. Cache preparation happens when enabling, selecting or reloading a list; drawing performs no file read, image decode or image scaling. Cold preparation is synchronous and may delay input on an SD card. Async prefetch is deferred, and its latency must be measured before animation is added.

## Verification

Focused simulator-feature compilation:

```sh
CARGO_TARGET_DIR=target/native-orbit-build \
  cargo check --locked --bin allium-launcher --features simulator
```

Headless rendering uses the project's existing MockPlatform and real App/EntryList/Orbit draw code, with real cover and screenshot artwork but clearly labeled fake CHD fixtures. It executes no emulator command. The fixture root is `out/allium-current/native-orbit-fixture`, with isolated database/state and read-only links to this checkout's fonts/config/locales. The proof requires that fixture directory name and rejects `/Volumes` roots.

```sh
ALLIUM_ORBIT=1 \
ALLIUM_SD_ROOT=target/native-orbit-fixture \
ALLIUM_BASE_DIR=target/native-orbit-fixture/.allium \
ALLIUM_DATABASE=target/native-orbit-fixture/allium.db \
ALLIUM_ORBIT_PROOF_DIR=target/native-orbit-evidence \
ALLIUM_ORBIT_PROOF_ART=../orbit-web-sim/dist/assets \
CARGO_TARGET_DIR=target/native-orbit-build \
  cargo test --locked --bin allium-launcher orbit -- --nocapture
```

The tests additionally check menu-underlay dirty settling, ancestor cache release, playlist folder classification, parser order/limits, modal ownership/cancellation, missing-file and override refusal, selected-disc Exec/shared identity, .ldci rollback, unchanged SRAM/state sentinels and ordinary single-disc routing. The test checks 640 × 480 output, selected index without tab switching, context-menu B closure, the emitted Exec program/core/game path, directory B, tab access, Settings/context shoulder ownership, X search command, no dirty loop after a static frame, long-title width, shelf bounds and PNG dimension rejection. PNGs and verbatim logs live beside the parent receipt.

The first proof exposed stale stock tab dirty flags; Orbit now queries its header/status/library owners. The next proof incorrectly called `GameInfo::load`, which invokes `swap-on.sh`; the host fixture has no hardware scripts. The corrected proof asserts the actual emitted Exec payload and never starts it.

Untested: ARM compilation, marker-based startup, native cold-art latency, framebuffer flush cost, physical evdev controls, actual launch-return/resume, saves and live device readability. Passing host rendering and routing tests does not establish any of those results. Keep the stock launcher until baseline capture, independent review and separately authorized reversible device proof.


## Multi-disc Play candidate

With Orbit enabled, A/Play discovers multi-disc M3U entries and opens a software-rendered modal. Up/Down selects Disc 1–8, A confirms, and B cancels. Active pickers are detected through the nested EntryList chain before Games-level shortcuts; X/search, Y/sort, shoulders, lateral navigation, Start and Menu are consumed without changing the underlying game. Existing shortcuts keep their original behavior when no picker is active. Missing files are labeled and cannot be confirmed. A cancellation emits no Exec and does not increment play count or write current-game/disc state. Single-disc entries retain normal launch routing. Context-menu Play reaches the same picker; Reset retains existing behavior.

The original entry path, display, artwork path and GameInfo identity remain unchanged. An outer `Example.m3u` directory is already classified as a Game by the official Entry::new code; this was asserted with the actual constructor. A matching `Example.m3u/Example.m3u` content file is preferred. Otherwise only a unique inner playlist with the same filename stem is accepted. Differing stems or ambiguity fail instead of changing shared-save identity. The normal RetroArch runner contains the same narrow normalization, so existing stock-launcher/alliumd two-argument launches can use an outer playlist folder without a daemon rebuild or JSON schema change. No gamelist metadata or folder rename is required for this fixture-backed route; device behavior remains untested.

The picker writes the supported RetroArch `<playlist stem>.ldci` startup record with zero-based index and the exact core-composed disc path. It launches the M3U, never a raw selected CHD. This is verified only for the bundled PCSX ReARMed and the reviewed shared save directory `Saves/CurrentProfile/saves` with save sorting disabled. Core/directory/game overrides, included settings, content-local saves or custom override directories cause safe refusal. The previous record is restored if launch preparation fails. Confirmation rechecks selected-file existence; cancellation leaves records intact.

The initial Exec adds `--choose-disc` to the existing runner, which appends transient settings: `savestate_auto_load=false`, `initial_disk_change_enable=true`, and `config_save_on_exit=false`. This starts the chosen disc without immediately restoring a prior-disc automatic state or persisting these temporary settings. SRAM and existing state files are preserved. GameInfo is saved with ordinary two-argument runner args before this initial-only flag is added, so daemon resume uses the normal autoload configuration. The `.ldci` record remains available to RetroArch's ordinary disk control. This feature requires both the reviewed launcher and runner patch; an ELF-only replacement cannot enable the transient startup behavior.

The bundled core's old parser strips CR/LF and skips only comments beginning in column zero. It does not normalize BOM, surrounding whitespace or absolute paths. Discovery displays those lines in order but refuses manual confirmation when their bytes are incompatible; it never rewrites playlists. At most eight discs and 64 KiB of playlist/settings input are allowed. Normal relative paths, LF/CRLF and column-zero comments are supported. Missing or malformed playlists fail visibly. Actual core execution and save compatibility still require device proof.

Primary-source pin and mechanism evidence: `out/allium-current/native-disc-startup-evidence.json` and its Markdown companion. The isolated runner argv check is `out/allium-current/native-disc-runner-check.py`; it invokes a stub, never RetroArch. Its log and native-disc-picker.png accompany the updated patch. No card/device files were written.
