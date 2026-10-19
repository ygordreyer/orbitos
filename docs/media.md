# Game media and metadata

OrbitOS reads artwork and metadata from the SD card. This repository supplies a media resolver, not a new scraper service or PC game manager. The existing scraper integration launches Collie.

## Files and lookup

Place `gamelist.xml` and media alongside a system's games under `Roms/<SYS>/`. Media files normally use the game filename without its final extension. PNG is tried before JPG in media directories.

| Requested image | Folder lookup, in order |
| --- | --- |
| Square cover | `media/covers`, `media/box2dfront` |
| Tall cover | `media/covers-tall` |
| Logo | `media/logos`, `media/marquees`, `media/wheel` |
| Background | `media/backgrounds`, `media/fanart` |
| Screenshot | `media/screenshots` |

Relative gamelist paths override folder lookup: `image`, then `thumbnail` for covers; `marquee` for logos; `fanart` for backgrounds. The resolver also accepts `miyoogamelist.xml`.

Directories can supply explicit `cover.png`, `icon.png`, `media/cover.png`, `media/system.png` or `logo.png` for covers. Directory logos can use `logo.png`, `icon.png` or `media/logo.png`; backgrounds can use `background.png` or `media/background.png`. Registered game containers can provide artwork to their member games.

Legacy fallbacks include `Imgs/<stem>.png` (also JPG, JPEG or GIF) for covers, `.orbit.png` for prepared covers and `.hero.png` for backgrounds. Lookup also checks ancestor layouts. Network catalog artwork uses the logical cache basename; a `.m3u` name loses its extension, while an ordinary directory keeps its full basename, including dotted tags.

Tall cover mode falls back to a fitted Square cover when no Tall source exists. Images are fitted or cropped according to the theme; runtime decoding and resizing can occur. Supplying images near device size reduces source work. Transparent PNG logos are supported. A missing logo falls back to title text, and themes can suppress a logo beside a visible cover with `logo_when_no_cover`.

The shared resolver consumes the kinds listed above plus prepared covers. Other producer conventions such as title screens, 3D boxes and back covers are not separate consumed kinds in this resolver.

## Metadata

| Field | Use |
| --- | --- |
| `path` | Game path relative to its gamelist |
| `name` | Display name |
| `desc` | Description |
| `releasedate` | Release date text, commonly `YYYYMMDDT000000` |
| `developer`, `publisher`, `genre`, `players` | Game information |
| `rating` | Rating text |
| `scraper` | Source label |
| `image`, `thumbnail`, `marquee`, `fanart` | Relative artwork overrides |

The shared metadata record does not expose `sortname`, `region`, `lang`, `family` or `scrapedate`. Do not rely on those fields appearing in Details. This read-only resolver makes no guarantee about unknown-tag preservation by external writers.

Gamelists stream one record at a time, with a 32 MiB file limit and 64 KiB record limit. Shared metadata retention is bounded to 4 MiB. Directory loads check fingerprints, and discarded records can be loaded again.

## Refresh and caching

After changing metadata, create `.allium/state/media-refresh` on the card. The launcher claims the marker, scans in background work and applies batches of at most 32 games during idle work. A new scan requires a new marker. Existing gameplay fields are preserved during metadata updates.

Artwork resolution and decoding run in background work. Selected and neighboring entries are prefetched; the UI polls bounded results and can initially draw without art. Fitted artwork can use a disk cache with content/transform keys and checksums, including pre-sized sources. The cache cap is 512 MiB. Artwork writes reserve 256 MiB free space plus an allocation/write margin, independently of the ROM-download reserve in the [network library](network-library.md).

External media tools should write complete temporary files and rename them into place so readers do not observe partial files. That is a recommendation for producers, not a claim that this repository controls every writer.
