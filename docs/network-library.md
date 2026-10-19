# Network library

OrbitOS can merge a manually provisioned PlayStation catalog into Library and Search without placeholder ROM files. It downloads registered payloads from an explicit HTTP endpoint and keeps them on the SD card until manually unloaded.

## Configuration

Create `.allium/state/network-library/config.json` and `catalog.json` on the card before starting the launcher. Neither a catalog nor a server is bundled. There is no automatic discovery or catalog synchronization.

A configuration example is:

```json
{"version":1,"library_id":"home-library","address":"192.168.1.10:8080"}
```

| Configuration field | Required value |
| --- | --- |
| `version` | Integer `1` |
| `library_id` | Must match the catalog's library ID |
| `address` | Literal IP and nonzero port, without a URL scheme |

Endpoints are restricted to private IPv4 LAN addresses or loopback addresses. IPv6 loopback is accepted with socket-address notation, for example `[::1]:8080`. Public addresses and hostnames are rejected. This is plain HTTP without a credential field; use a trusted private network.

Each configuration file has a 1 MiB read limit. A confirmed absent profile leaves the ordinary library active. Invalid configured state blocks the configured PS namespace and reports an error; other tabs remain accessible. Repair the files and restart the launcher.

## Catalog schema

| Top-level field | Type and limits |
| --- | --- |
| `version` | Integer `1` |
| `library_id` | Nonempty ID, at most 64 ASCII letters, digits, hyphens or underscores |
| `revision` | 64 lowercase hexadecimal characters |
| `games` | Array of at most 512 game records |

| Game field | Type and meaning |
| --- | --- |
| `id` | Unique ID with the same syntax as `library_id` |
| `display_title` | Nonempty UTF-8 string, at most 512 bytes |
| `platform` | String `PS` |
| `bundle_revision` | 64 lowercase hexadecimal characters |
| `game_path` | Logical library identity relative to the SD root |
| `cache_path` | Installed file or directory relative to the SD root |
| `install_kind` | String `file` or `directory` |
| `launch_path` | Registered payload path used to launch |
| `files` | Array of 1 to 128 payload records |
| `discs` | Array of at most eight distinct payload IDs, all registered in `files`; use an empty array for none |

| Payload field | Type and meaning |
| --- | --- |
| `id` | Unique within the game, with the same ID syntax |
| `relative_path` | Empty for a single-file install; otherwise a safe path beneath `cache_path` |
| `bytes` | Positive integer, at most 4294967295 |
| `sha256` | Full payload SHA-256, 64 lowercase hexadecimal characters |
| `role` | String `rom`, `cue` or `playlist`; CUE/M3U extensions must match their descriptor role |
| `http_route` | Exactly `/v1/files/<game-id>/<bundle_revision>/<payload-id>` |

Paths must remain under `Roms/PS`, with no absolute paths, empty components, dot components, parent traversal or backslashes. Protected components such as `Imgs`, `Saves`, `BIOS` and `Guides` are rejected. Allowed payload extensions are BIN, CUE, CHD, ISO, PBP and M3U. Case-fold collisions and overlapping ownership across games are rejected.

For `file`, register exactly one payload with an empty `relative_path`; it installs at `cache_path`. Usually `game_path` equals `launch_path`. A directory install can instead use an outer `.m3u` identity whose inner launch playlist has the same basename. Register every track and disc required by the game. Catalog validation is implemented in `crates/allium-launcher/src/network_library.rs`.

Catalog titles are retained after caching. Legacy network artwork uses `Roms/PS/Imgs/<cache-basename>.png`, `.orbit.png` and `.hero.png`. A `.m3u` outer name loses only that extension; an ordinary directory keeps its full name. Optional arbitrary catalog artwork overrides are not consumed. See [media](media.md).

## Download and launch

Play can download and verify the complete bundle before continuing. Download can also fill the cache without pending autoplay. B cancels transfer or verification and clears pending launch actions. Existing unmanaged payload requires explicit Verify and register confirmation before adoption.

The HTTP server must return exact content lengths and a quoted SHA-256 `ETag`. New transfers require status 200. A partial transfer resumes with `Range: bytes=<offset>-` and `If-Range` using that ETag; the response must be 206 with the exact `Content-Range`. Encoded and chunked bodies are rejected.

Downloads verify the full streamed SHA-256 at completion before installation is published. A cached launch first checks recorded catalog identity, file/stat information and sampled fingerprints. If that fast path cannot establish readiness, it falls back to full verification. Cached payloads are not fully re-hashed on every launch.

After valid headers, the first body byte can take up to 120 seconds. Later gaps and the final close have a 10-second timeout. Cancellation remains available. Timeouts do not retry automatically.

## Space and unload

For each payload, round its size up to the filesystem allocation unit, then subtract already verified partial allocation. Required free space is the sum of those additional allocations plus `2_162_688 * 1024` bytes of reserve and 65536 bytes of metadata margin. The reserve is 2 GiB plus 64 MiB; the complete fixed allowance is 2 GiB plus 64 MiB plus 64 KiB.

A storage error offers Manage storage, which is also available from a network game's context menu. Up/Down selects a game; X switches largest/least-recently-played ordering; A opens unload confirmation; B returns. There is no automatic eviction or automatic retry after unload.

Unload verifies registered files, rejects unexpected contents, protects the current resume game and records a durable unload intent. An interrupted unload stays non-Ready until explicitly resumed through Unload. Only owned payload and its known cache metadata/directories are removed. Artwork, catalog cards, saves, BIOS, guides and play history remain.

The cache uses one writer lock and anchored filesystem handles. Do not edit registered ROM paths during a confirmed unload. External name replacement during the final validation/removal interval is unsupported.

## Download completion diagnostics

When an install target is missing, Download remains available even if the retained index says Cached. After transfer, the dialog shows Verifying while the worker checks hashes and publishes the install.

On the device, enable tracing before restarting the launcher:

```sh
touch /tmp/allium-perf-trace
```

Reproduce the download and collect `/tmp/allium-network-trace.log` and `/tmp/allium-perf-trace.log` before reboot. The network log records worker delivery, polling, modal changes, draw/flush returns and event-loop wake causes. `monotonic_us`, `pid` and `thread` help correlate events across threads. Concurrent send and receive log lines can appear in either order.

Both logs are capped at 256 KiB. The network log rolls over; the performance log stops at its cap. Remove the flag and restart the launcher to disable tracing. Flag detection is cached for the process lifetime. A successful flush return establishes that the platform call returned; it does not prove that the LCD physically updated.
