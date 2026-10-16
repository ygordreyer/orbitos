# Game media and metadata contract

One layout shared by the launcher, the scraper and the PC game manager. It follows ES-DE and Batocera conventions so existing scrapes keep working, with two additions ES-DE lacks: a separate tall cover and pre-sized device images.

## Location

Per system folder on the SD card: `/mnt/SDCARD/Roms/<SYS>/`.

- `gamelist.xml`: metadata, ES-DE format (one `<game>` per entry).
- `media/<kind>/<stem>.<ext>`: images. `<stem>` is the ROM file name without its last extension (`Chrono Trigger (USA).sfc` gives `Chrono Trigger (USA)`). For a game folder or `.m3u`, the folder name or the m3u name without extension. `<ext>` is `png` or `jpg`; the launcher tries `png` first.

## Image kinds

| Kind | Content | Device size | Format |
|---|---|---|---|
| `covers` | Front box art, native aspect (the Square frame source) | long side at most 480 px | PNG or JPG |
| `covers-tall` | Portrait front cover, 2:3 aspect (the Tall frame source), scraped from its own source, never a crop of `covers` | 320x480 | PNG or JPG |
| `logos` | Clear logo (wheel), transparent background | fits 480x200 | PNG with alpha |
| `backgrounds` | Fanart or key art | 640x480, center cropped | JPG |
| `screenshots` | In-game screenshot | at most 640x480 | PNG or JPG |
| `titlescreens` | Title screen | at most 640x480 | PNG or JPG |
| `box3d` | 3D box render | long side at most 480 px | PNG with alpha |
| `backcovers` | Back of the box | long side at most 480 px | JPG |
| `physicalmedia` | Cartridge or disc | long side at most 480 px | PNG with alpha |
| `miximages` | Composite preview (legacy, read only) | as found | as found |

All device images are pre-sized by the scraper or the PC manager. The launcher never has to downscale a large source at runtime.

## Lookup order (launcher)

- Square cover: `media/covers`, then `media/box2dfront` (Skraper), then `Imgs/<stem>.png` (legacy Allium).
- Tall cover: `media/covers-tall`. When it is missing, the theme draws the Square cover inside the tall frame (fit, never stretch).
- Logo: `media/logos`, then `media/marquees` (ES-DE name), then `media/wheel` (Batocera name). When none exists, the theme draws the title text.
- Background: `media/backgrounds`, then `media/fanart`. When none exists, the theme draws its own background.
- A `gamelist.xml` media tag (`<image>`, `<thumbnail>`, `<marquee>`, `<fanart>`) with a relative path overrides the folder lookup for that kind.

## Metadata (gamelist.xml)

ES-DE fields: `path`, `name`, `sortname`, `desc`, `rating` (0 to 1), `releasedate` (`YYYYMMDDT000000`), `developer`, `publisher`, `genre`, `players`, `region`, `lang`. Extra fields the launcher may show: `family` (series), `scraper` (source name), `scrapedate`. Unknown tags are preserved by every writer.

## Writers

- Writers replace one file at a time through a temporary name and rename, so the launcher never reads a half-written image or gamelist.
- Writers never touch ROMs, saves, BIOS or `Imgs/` (legacy, read only).
- The launcher checks gamelist fingerprints when a directory loads. `/mnt/SDCARD/.allium/state/media-refresh` queues a background rescan and is consumed when claimed. Invalid systems are logged and skipped; another scan requires a new marker. Refresh applies at most 32 games per idle tick, after input.
- Frame preparation reads cached media only. Idle work prefetches the selected game and its neighbours, including separate Square and Tall covers and logos. Artwork uses the existing 3 MiB cache budget.
- Shared metadata records have a 4 MiB retained-byte budget with eviction. Game rows do not copy scraper records. Gamelists up to 32 MiB stream one record at a time (64 KiB maximum per record); larger or malformed input produces a logged error. Evicted records can be loaded again during idle work.
