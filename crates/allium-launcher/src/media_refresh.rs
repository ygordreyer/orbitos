use crate::consoles::ConsoleMapper;
use anyhow::Result;
use common::{
    database::{Database, NewGame},
    locale::Locale,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
};

const BATCH: usize = 32;
enum Update {
    Games(Vec<NewGame>),
    Source(PathBuf, i64),
    Failed(String),
    Finished,
}
struct Refresh {
    root: PathBuf,
    updates: Receiver<Update>,
}
thread_local! {static JOB: std::cell::RefCell<Option<Refresh>> = const {std::cell::RefCell::new(None)};}

// Disk traversal and XML decoding never run on the UI thread. The bounded channel
// backpressures the reader while input preempts the idle branch.
fn start(root: &Path) -> Refresh {
    let (tx, updates) = mpsc::sync_channel(2);
    let root = root.to_owned();
    let worker_root = root.clone();
    std::thread::spawn(move || {
        let run = || -> Result<()> {
            let mut directories = vec![worker_root.join("Roms")];
            while let Some(path) = directories.pop() {
                let source = ["gamelist.xml", "miyoogamelist.xml"]
                    .into_iter()
                    .map(|n| path.join(n))
                    .find(|p| p.is_file());
                if let Some(source) = source {
                    let fingerprint = common::media::fingerprint(&fs::metadata(&source)?);
                    let mut batch = Vec::with_capacity(BATCH);
                    let mut disconnected = false;
                    let parsed = common::media::stream_gamelist(&source, |data| {
                        let Ok(game_path) = path.join(&data.path).canonicalize() else {
                            return;
                        };
                        let image = [&data.image, &data.thumbnail]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .map(|s| path.join(s))
                            .find(|p| p.is_file());
                        batch.push(NewGame {
                            path: game_path,
                            name: data.name,
                            image,
                            core: None,
                            rating: data
                                .rating
                                .parse::<f32>()
                                .ok()
                                .map(|n| (n.clamp(0., 1.) * 10.) as u8),
                            release_date: chrono::NaiveDateTime::parse_from_str(
                                &data.releasedate,
                                "%Y%m%dT%H%M%S",
                            )
                            .ok()
                            .map(|d| d.date()),
                            developer: (!data.developer.is_empty()).then_some(data.developer),
                            publisher: (!data.publisher.is_empty()).then_some(data.publisher),
                            genres: data
                                .genre
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .collect(),
                            favorite: false,
                        });
                        if batch.len() == BATCH
                            && tx
                                .send(Update::Games(std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(BATCH),
                                )))
                                .is_err()
                        {
                            disconnected = true;
                        }
                    });
                    if disconnected {
                        return Ok(());
                    }
                    match parsed {
                        Ok(()) => {
                            if !batch.is_empty() {
                                tx.send(Update::Games(batch))?;
                            }
                            tx.send(Update::Source(path.clone(), fingerprint))?;
                        }
                        Err(error) => {
                            tx.send(Update::Failed(format!("{}: {error:#}", source.display())))?;
                        }
                    }
                }
                for entry in fs::read_dir(path)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir()
                        && !matches!(
                            entry.file_name().to_str(),
                            Some("media" | "Imgs" | "Guides")
                        )
                    {
                        directories.push(entry.path());
                    }
                }
            }
            Ok(())
        };
        if let Err(error) = run() {
            let _ = tx.send(Update::Failed(format!("{error:#}")));
        }
        let _ = tx.send(Update::Finished);
    });
    Refresh { root, updates }
}

pub(crate) fn pending() -> bool {
    JOB.with(|job| job.borrow().is_some())
}

/// One database batch per idle tick. Claim the marker before any parse, including
/// failed ones, so malformed systems cannot trigger recurring full-library scans.
pub(crate) fn consume(
    root: &Path,
    database: &Database,
    _mapper: &ConsoleMapper,
    _locale: &Locale,
) -> Result<bool> {
    JOB.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|job| job.root != root) {
            *slot = None;
        }
        if slot.is_none() {
            let marker = root.join(".allium/state/media-refresh");
            match fs::remove_file(&marker) {
                Ok(()) => {
                    *slot = Some(start(root));
                    return Ok(false);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
        match slot.as_ref().unwrap().updates.try_recv() {
            Ok(Update::Games(games)) => database.update_games(&games)?,
            Ok(Update::Source(path, fingerprint)) => {
                database.set_gamelist_fingerprint(&path, fingerprint)?
            }
            Ok(Update::Failed(error)) => {
                log::error!("Media refresh failed (marker consumed): {error}")
            }
            Ok(Update::Finished) | Err(TryRecvError::Disconnected) => {
                *slot = None;
                crate::view::orbit::invalidate_art();
                crate::view::details::invalidate_metadata();
                return Ok(true);
            }
            Err(TryRecvError::Empty) => {}
        }
        Ok(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_refresh_applies_at_most_one_batch_and_never_reopens_xml() {
        let root = std::env::temp_dir().join(format!(
            "allium-review-refresh-batch-{}",
            std::process::id()
        ));
        let dir = root.join("Roms/ONE");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(root.join(".allium/state")).unwrap();
        let root = root.canonicalize().unwrap();
        let dir = root.join("Roms/ONE");
        let mut xml = String::from("<gameList>");
        for n in 0..160 {
            fs::write(dir.join(format!("G{n}.sfc")), b"fixture").unwrap();
            xml.push_str(&format!(
                "<game><path>./G{n}.sfc</path><name>Game {n}</name></game>"
            ));
        }
        xml.push_str("</gameList>");
        let source = dir.join("gamelist.xml");
        fs::write(&source, xml).unwrap();
        let db = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let locale = Locale::new("en");
        fs::write(root.join(".allium/state/media-refresh"), b"").unwrap();
        assert!(!consume(&root, &db, &mapper, &locale).unwrap());
        assert!(pending());
        let mut previous = 0;
        let mut replaced = false;
        let mut finished = false;
        for _ in 0..10000 {
            finished = consume(&root, &db, &mapper, &locale).unwrap();
            let count = db.select_games_in_directory(&dir).unwrap().len();
            assert!(
                count - previous <= BATCH,
                "one idle tick applied {} games",
                count - previous
            );
            if count > 0 && !replaced {
                fs::write(dir.join("replacement.xml"), b"<broken").unwrap();
                fs::rename(dir.join("replacement.xml"), &source).unwrap();
                replaced = true;
            }
            previous = count;
            if finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(finished && replaced);
        assert!(!pending());
        assert_eq!(
            previous, 160,
            "refresh must keep its single open XML stream"
        );
        fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
    #[test]
    fn review_refresh_is_batched_parses_once_and_consumes_failed_marker() {
        let root = std::env::temp_dir().join(format!("allium-media-marker-{}", std::process::id()));
        fs::create_dir_all(root.join(".allium/state")).unwrap();
        let root = root.canonicalize().unwrap();
        let db = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::new();
        let locale = Locale::new("en");
        for system in ["ONE", "TWO"] {
            let dir = root.join("Roms").join(system);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("Game.sfc"), b"fixture").unwrap();
            fs::write(dir.join("gamelist.xml"),"<gameList><game><path>./Game.sfc</path><name>Updated game</name><releasedate>19981203T000000</releasedate></game></gameList>").unwrap();
            db.set_gamelist_fingerprint(
                &dir,
                common::media::fingerprint(&fs::metadata(dir.join("gamelist.xml")).unwrap()),
            )
            .unwrap();
        }
        let marker = root.join(".allium/state/media-refresh");
        fs::write(&marker, b"").unwrap();
        assert!(!consume(&root, &db, &mapper, &locale).unwrap());
        assert!(!marker.exists());
        assert!(
            db.select_games_in_directory(&root.join("Roms/ONE"))
                .unwrap()
                .is_empty()
        );
        let drain = || {
            for _ in 0..10000 {
                if consume(&root, &db, &mapper, &locale).unwrap() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("refresh did not finish");
        };
        drain();
        for system in ["ONE", "TWO"] {
            let games = db
                .select_games_in_directory(&root.join("Roms").join(system))
                .unwrap();
            assert_eq!(games.len(), 1);
            assert_eq!(games[0].name, "Updated game");
            assert_eq!(games[0].release_date.unwrap().to_string(), "1998-12-03");
        }
        assert!(!consume(&root, &db, &mapper, &locale).unwrap());
        fs::write(root.join("Roms/TWO/gamelist.xml"), "<broken").unwrap();
        fs::write(&marker, b"").unwrap();
        let result = consume(&root, &db, &mapper, &locale);
        assert!(
            !marker.exists(),
            "failed refresh must consume marker: {result:?}"
        );
        drain();
        assert!(!consume(&root, &db, &mapper, &locale).unwrap());
        fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}

struct MarkerProbe {
    root: PathBuf,
    result: Receiver<std::io::Result<bool>>,
}
thread_local! {
    static MARKER_PROBE: std::cell::RefCell<Option<MarkerProbe>> = const { std::cell::RefCell::new(None) };
}
/// The routine idle poll must not wait on an SD-card stat when no refresh is requested.
pub(crate) fn marker_ready(root: &Path) -> Result<Option<bool>> {
    MARKER_PROBE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|probe| probe.root != root) {
            *slot = None;
        }
        if slot.is_none() {
            let marker = root.join(".allium/state/media-refresh");
            let (send, result) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("allium-media-marker".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    let _ = send.send(marker.try_exists());
                })?;
            *slot = Some(MarkerProbe {
                root: root.into(),
                result,
            });
            return Ok(None);
        }
        match slot.as_ref().unwrap().result.try_recv() {
            Ok(found) => {
                *slot = None;
                Ok(Some(found?))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                *slot = None;
                anyhow::bail!("Media marker worker disconnected")
            }
        }
    })
}

#[cfg(test)]
mod marker_probe_tests {
    use super::*;
    #[test]
    fn asynchronous_marker_probe_keeps_claim_and_refresh_on_the_existing_path() {
        let root = std::env::temp_dir().join(format!("allium-marker-probe-{}", std::process::id()));
        fs::create_dir_all(root.join(".allium/state")).unwrap();
        let marker = root.join(".allium/state/media-refresh");
        fs::write(&marker, []).unwrap();
        assert_eq!(marker_ready(&root).unwrap(), None);
        let started = std::time::Instant::now();
        loop {
            if let Some(found) = marker_ready(&root).unwrap() {
                assert!(found);
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(marker.exists(), "probe must not claim the marker");
        fs::remove_dir_all(root).unwrap();
    }
}
