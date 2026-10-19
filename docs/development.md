# Development

OrbitOS retains the Allium workspace and build targets. A complete checkout needs static files, migrations and submodules, including files hidden by any sparse-checkout configuration.

## Toolchain

| Component | Requirement |
| --- | --- |
| Rust | Nightly, edition 2024; `rust-toolchain.toml` does not pin a date |
| Device Rust build | cargo-zigbuild and Zig; `armv7-unknown-linux-gnueabihf`, glibc 2.28 |
| Device CPU flags | Cortex-A7, NEON and VFP4 in `.cargo/config.toml` |
| C/C++ components | Docker image `mholdg16/miyoomini-toolchain:latest` for RetroArch, Dropbear and stripping |
| Collie frontend | Node 22 and pnpm 9, as selected in the build workflow |
| Other commands | make, git, rsync, clang/libclang, patchelf, wget, tar, unzip and zip |

Initialize submodules with `git submodule update --init --recursive`. On macOS, `scripts/setup-mac.sh` installs SDL2, Rust/ARM target, Zig, patchelf and cargo-zigbuild and initializes submodules. It does not install every full-package dependency above. Ensure Cargo uses the nightly toolchain when another system Rust installation precedes rustup on PATH.

Nightly, the Docker `latest` tag and the fetched upstream theme repository are not pinned inputs. A rebuild can therefore differ when those inputs change.

## Make targets

| Command | Scope |
| --- | --- |
| `make all` | Assemble static files, device binaries, RetroArch, service dependencies, themes and migrations in `dist/`, then strip binaries |
| `make dist` | Copy tracked static files and the repository's native themes into `dist/` |
| `make build` | Build device Rust binaries and patch `myctl` library references |
| `make debug` | Build debug device Rust binaries |
| `make package-build` | Copy built binaries into an existing distribution layout and write `.allium/version.txt` |
| `make migrations` | Package migration archives |
| `make strip-all` | Strip device files through Docker |
| `make simulator bin=allium-launcher` | Run the windowed launcher simulator |
| `make simulator bin=allium-menu` | Run the windowed in-game menu simulator |
| `make toolchain` | Open a shell in the Docker toolchain |
| `make clean` | Remove `dist/` and clean the RetroArch patch build |

`alliumd` embeds the `allium-menu` library. `package-build` does not deploy a separate menu binary. Shared rendering, platform, database and media code is in `common`.

The simulator target copies static content and fetches upstream stylesheet themes. To use the four native themes there, also run `rsync -a themes/ simulator/Themes/`. The simulator does not verify device input timing, framebuffer behavior or hardware power handling.

## Host tests

From the repository root, run the four main packages with the host target reported by `rustc -vV`:

```sh
cargo test -p common -p allium-launcher -p allium-menu -p alliumd --target aarch64-apple-darwin --no-fail-fast
```

Replace `aarch64-apple-darwin` on another host. These packages build without device features, using the mock platform. Some existing capture/integration tests require explicitly provisioned fixture roots and environment variables; a bare run can fail if those are absent. Tests that bind loopback sockets also require permission to create sockets. The command above is a test inventory, not a claim that every existing test passes without setup.

The reader pagination test uses tracked themes and skips when `ALLIUM_SD_ROOT` is unset. Artwork fixtures are synthetic PNGs under `test-fixtures/synthetic-art/`. Rebuild them with `python3 scripts/gen-synthetic-art.py`, which uses only the Python standard library and preserves fixture dimensions and PNG color types.

Optional capture inputs are listed below. Use disposable fixture directories, not a mounted card. General mock-platform fixture tests use `ALLIUM_SD_ROOT`, `ALLIUM_BASE_DIR` and `ALLIUM_DATABASE` to select their isolated card, static assets and database. Tests that assert a `native-orbit-fixture` root require that basename. Explicit capture outputs use their individual output variables.

| Variable | Purpose |
| --- | --- |
| `ORBIT_WEB_SIM_DIST` | External web build's `dist` directory for the exporter and ignored parity capture |
| `PARITY_AUDIT_ROOT` | Capture input directory with `vm-models.json` and `model-assets.json` |
| `PARITY_A_OUTPUT` | Output directory for `parity_a_capture_all_options` |
| `ALLIUM_UX4_PACK` | Optional external descriptor for legacy ignored measurements |
| `ALLIUM_PARITY_B_FRAMES` | Optional rendered parity-frame output directory |
| `ALLIUM_UPDATE_FRAME_HASHES` | Update the tracked theme frame hashes when explicitly running the frame-hash test |
| `ORBITOS_BUILD_ID` | Optional hexadecimal build ID override; otherwise derived from Git |

The parity capture stays ignored by default and returns without capture when `ORBIT_WEB_SIM_DIST` is unset, even when explicitly selected with `--ignored`. Its other inputs must be supplied to perform a capture.

## Optional asset maintenance

The checked-in native themes build without an external web project. To re-export them, supply a built web distribution containing `theme-layout.js`, `themes/theme.schema.json`, the four theme descriptors and their assets:

```sh
node scripts/export-web-themes.cjs /path/to/web/dist themes
ORBIT_WEB_SIM_DIST=/path/to/web/dist node scripts/export-web-themes.cjs
```

There is no implicit sibling-checkout dependency. The first positional argument is the web `dist` directory; the optional second argument is the output directory (default `themes`). The `--logo-headers-only` and `--folder-labels-only` maintenance modes edit the existing native output without needing web inputs.

`scripts/instance-theme-fonts.py <source-directory>` optionally regenerates tracked Nunito/Roboto weight faces with Python fontTools. It needs `Nunito-Variable.ttf` and `Roboto-Variable.ttf` in the supplied directory. Normal builds use the tracked faces and their license notices.

## Packaging layout

| Package directory | Contents |
| --- | --- |
| `.allium` | Launcher, daemon, service binaries, config, fonts, locales, scripts, splash, state template and migrations |
| `.tmp_update` | Boot updater, device support libraries and small command binaries |
| `Apps` | App packages including Activity Tracker and Screenshot Viewer |
| `RetroArch` | Executable, cores and configuration |
| `Themes` | Upstream stylesheet themes and the four repository native packs/assets |
| `BIOS`, `Roms`, `Saves` | Directory templates and static content; supply your own game and BIOS files |

`make dist` refreshes native descriptors even when `dist/Themes` already exists. The full build fetches upstream themes once per distribution tree, recorded by `Themes/.upstream`, and then overlays native packs. `package-build` writes the exact Git tag into `version.txt`, or `nightly-<short-hash>` when there is no matching tag.

`make all` does not create an archive. After the build, the inherited workflow packages these directories. To create a locally named OrbitOS archive:

```sh
cd dist
zip -r ../orbitos-v0.1.0-alpha.zip .allium .tmp_update RetroArch Apps BIOS Roms Saves Themes
```

The inherited CI/release workflow still names its archive `allium-armv7-unknown-linux-gnueabihf.zip`. `dist/Wallpapers` is copied from static content but is outside that archive's directory list.

## Deployment

Set `SDCARD_PATH` in a git-ignored `local.mk`, using `local.mk.example` as a template, or supply it to make. Both deployment targets use an already-built `dist/`; neither builds first.

`make deploy` overlays `.allium`, `.tmp_update`, `Apps`, `RetroArch` and `Themes`. It does not delete destination-only files. Its `--update` option can leave newer destination files unchanged.

`make deploy-all` mirrors only `.allium`, `.tmp_update`, `Apps` and `Themes` with `--delete` and `--update`. It omits RetroArch, BIOS, Roms and Saves. Destination-only files inside the selected directories can be deleted. It is not a fresh-card installation command.

For a fresh card, copy the complete package, including hidden directories, as described in the [installation instructions](../README.md#installation). Rollback is a manual backup restore; there is no rollback make target.
