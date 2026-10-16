//! Pure catalog/cache planning; no network, filesystem mutation or launcher hook.
#![allow(dead_code)]
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub const RESERVE_BYTES: u64 = 2_162_688 * 1024;
pub const METADATA_MARGIN: u64 = 64 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Catalog {
    pub version: u32,
    pub library_id: String,
    pub revision: String,
    pub games: Vec<NetworkGame>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NetworkGame {
    pub id: String,
    pub display_title: String,
    pub platform: String,
    pub bundle_revision: String,
    pub game_path: String,
    pub cache_path: String,
    pub install_kind: String,
    pub launch_path: String,
    pub files: Vec<Payload>,
    pub discs: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
    pub id: String,
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
    pub role: String,
    pub http_route: String,
}
fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn relative(value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty()
            && !value.contains(['\\', '\0'])
            && !value
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".."),
        "Unsafe relative path"
    );
    let path = Path::new(value);
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "Absolute/traversing path"
    );
    Ok(path.to_path_buf())
}
fn rom_path(value: &str) -> Result<PathBuf> {
    let path = relative(value)?;
    ensure!(
        path.starts_with("Roms/PS") && path.components().count() >= 3,
        "Unsupported/protected root"
    );
    ensure!(
        path.components()
            .skip(2)
            .all(|c| !["imgs", "saves", "bios", "guides"]
                .contains(&c.as_os_str().to_string_lossy().to_lowercase().as_str())),
        "Protected path"
    );
    Ok(path)
}
fn folded(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}
fn component_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn claim_ownership(
    claims: &mut Vec<(String, String, bool)>,
    game_id: &str,
    path: &Path,
    cache_root: bool,
) -> Result<()> {
    let path = folded(path);
    ensure!(
        claims
            .iter()
            .all(|(claimed, owner, claimed_root)| owner == game_id
                || (claimed != &path
                    && !((cache_root || *claimed_root) && component_overlap(claimed, &path)))),
        "Cross-game ownership collision"
    );
    claims.push((path, game_id.to_owned(), cache_root));
    Ok(())
}
impl NetworkGame {
    pub fn targets(&self) -> Result<Vec<PathBuf>> {
        let cache = rom_path(&self.cache_path)?;
        self.files
            .iter()
            .map(|file| {
                let path = if self.install_kind == "file" {
                    ensure!(
                        self.files.len() == 1 && file.relative_path.is_empty(),
                        "Invalid single-file target"
                    );
                    cache.clone()
                } else {
                    cache.join(relative(&file.relative_path)?)
                };
                rom_path(
                    path.to_str()
                        .ok_or_else(|| anyhow::anyhow!("Non UTF8 path"))?,
                )?;
                ensure!(
                    ["bin", "cue", "chd", "iso", "pbp", "m3u"].contains(
                        &path
                            .extension()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase()
                            .as_str()
                    ),
                    "Unsupported executable/metadata payload"
                );
                Ok(path)
            })
            .collect()
    }
}
impl Catalog {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && opaque(&self.library_id)
                && digest(&self.revision)
                && self.games.len() <= 512,
            "Invalid catalog"
        );
        let mut ids = HashSet::new();
        let mut targets = HashSet::new();
        let mut identities = HashSet::new();
        let mut ownership = Vec::new();
        for game in &self.games {
            ensure!(
                opaque(&game.id)
                    && ids.insert(&game.id)
                    && digest(&game.bundle_revision)
                    && game.platform == "PS"
                    && ["file", "directory"].contains(&game.install_kind.as_str()),
                "Invalid game"
            );
            let identity = rom_path(&game.game_path)?;
            let cache_root = rom_path(&game.cache_path)?;
            ensure!(
                identities.insert(folded(&identity)),
                "Duplicate game identity"
            );
            claim_ownership(&mut ownership, &game.id, &identity, false)?;
            claim_ownership(&mut ownership, &game.id, &cache_root, true)?;
            ensure!(
                !game.display_title.is_empty()
                    && game.display_title.len() <= 512
                    && !game.files.is_empty()
                    && game.files.len() <= 128
                    && game.discs.len() <= 8,
                "Invalid game bounds"
            );
            let paths = game.targets()?;
            ensure!(
                game.game_path == game.launch_path
                    || (game.game_path == game.cache_path
                        && game.install_kind == "directory"
                        && Path::new(&game.cache_path)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("m3u"))
                        && Path::new(&game.launch_path)
                            == Path::new(&game.cache_path)
                                .join(Path::new(&game.cache_path).file_name().unwrap())),
                "Incompatible Allium identity/launch path"
            );
            ensure!(
                paths.contains(&rom_path(&game.launch_path)?),
                "Unregistered launch path"
            );
            let mut files = HashSet::new();
            for (file, path) in game.files.iter().zip(paths) {
                ensure!(
                    (file.role == "cue")
                        == path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
                        && (file.role == "playlist")
                            == path
                                .extension()
                                .is_some_and(|extension| extension.eq_ignore_ascii_case("m3u")),
                    "Descriptor role mismatch"
                );
                ensure!(
                    opaque(&file.id)
                        && files.insert(&file.id)
                        && digest(&file.sha256)
                        && file.bytes > 0
                        && file.bytes <= u32::MAX.into()
                        && ["rom", "cue", "playlist"].contains(&file.role.as_str()),
                    "Invalid payload"
                );
                ensure!(targets.insert(folded(&path)), "FAT case-fold collision");
                claim_ownership(&mut ownership, &game.id, &path, false)?;
                ensure!(
                    file.http_route
                        == format!("/v1/files/{}/{}/{}", game.id, game.bundle_revision, file.id),
                    "Non-ID route"
                );
            }
            ensure!(
                game.discs.iter().all(|id| files.contains(id))
                    && game.discs.iter().collect::<HashSet<_>>().len() == game.discs.len(),
                "Unregistered/duplicate discs"
            );
        }
        Ok(())
    }
    pub fn merge_paths(&self, root: &Path, local: &[PathBuf]) -> Result<Vec<PathBuf>> {
        self.validate()?;
        let mut paths = local.to_vec();
        let mut seen = local
            .iter()
            .map(|p| p.to_string_lossy().to_lowercase())
            .collect::<HashSet<_>>();
        for game in &self.games {
            let path = root.join(rom_path(&game.game_path)?);
            if seen.insert(path.to_string_lossy().to_lowercase()) {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}
#[derive(Debug, PartialEq)]
pub enum CacheState {
    NetworkOnly,
    Partial,
    Verifying,
    Ready {
        game_id: String,
        bundle_revision: String,
        verified_files: usize,
    },
}
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum LaunchRoute {
    Play,
    ContextPlay,
    Reset,
    Resume,
}
impl CacheState {
    pub fn permit_launch(&self, game: &NetworkGame, _route: LaunchRoute) -> bool {
        matches!(self,Self::Ready{game_id,bundle_revision,verified_files} if game_id==&game.id && bundle_revision==&game.bundle_revision && *verified_files==game.files.len())
    }
}
#[derive(Debug, PartialEq)]
pub struct SpacePlan {
    pub additional_bytes: u64,
    pub required_free_bytes: u64,
    pub shortage_bytes: u64,
}
#[derive(Debug, Clone)]
pub struct PartialAllocation {
    pub game_id: String,
    pub bundle_revision: String,
    pub file_id: String,
    pub sha256: String,
    pub allocated_bytes: u64,
}
pub fn plan_space(
    game: &NetworkGame,
    unit: u64,
    allocated_partial: &[PartialAllocation],
    free: u64,
) -> Result<SpacePlan> {
    ensure!(unit > 0, "Invalid allocation unit");
    let mut partial_ids = HashSet::new();
    for partial in allocated_partial {
        ensure!(
            partial.game_id == game.id
                && partial.bundle_revision == game.bundle_revision
                && partial_ids.insert(&partial.file_id),
            "Stale/duplicate partial identity"
        );
        ensure!(
            game.files
                .iter()
                .any(|file| file.id == partial.file_id && file.sha256 == partial.sha256),
            "Unknown partial payload"
        );
    }
    let mut total = 0u64;
    for file in &game.files {
        let allocated = file
            .bytes
            .checked_add(unit - 1)
            .and_then(|v| v.checked_div(unit))
            .and_then(|v| v.checked_mul(unit))
            .ok_or_else(|| anyhow::anyhow!("Allocation overflow"))?;
        let partial = allocated_partial
            .iter()
            .find(|partial| partial.file_id == file.id)
            .map_or(0, |partial| partial.allocated_bytes);
        ensure!(
            partial <= allocated && partial % unit == 0,
            "Unverified/invalid partial allocation"
        );
        total = total
            .checked_add(allocated - partial)
            .ok_or_else(|| anyhow::anyhow!("Total overflow"))?;
    }
    let required = total
        .checked_add(RESERVE_BYTES)
        .and_then(|v| v.checked_add(METADATA_MARGIN))
        .ok_or_else(|| anyhow::anyhow!("Reserve overflow"))?;
    Ok(SpacePlan {
        additional_bytes: total,
        required_free_bytes: required,
        shortage_bytes: required.saturating_sub(free),
    })
}
#[derive(Debug, Clone)]
pub struct OwnedFile {
    pub path: PathBuf,
    pub expected_sha256: String,
    pub observed_sha256: String,
    pub expected_bytes: u64,
    pub observed_bytes: u64,
    pub regular: bool,
    pub symlink_component: bool,
}
#[derive(Debug, Clone)]
pub struct OwnedCache {
    pub game_id: String,
    pub bundle_revision: String,
    pub files: Vec<OwnedFile>,
    pub unexpected_in_root: bool,
}
pub fn plan_unload(
    root: &Path,
    game: &NetworkGame,
    ownership: &OwnedCache,
    resume_game_path: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    ensure!(
        ownership.game_id == game.id && ownership.bundle_revision == game.bundle_revision,
        "Stale ownership identity"
    );
    let owned = &ownership.files;
    let identity = root.join(rom_path(&game.game_path)?);
    ensure!(
        resume_game_path.is_none_or(|path| path.to_string_lossy().to_lowercase()
            != identity.to_string_lossy().to_lowercase()),
        "Resume-protected game"
    );
    ensure!(
        !ownership.unexpected_in_root,
        "Unexpected directory contents"
    );
    let targets = game
        .targets()?
        .into_iter()
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    ensure!(owned.len() == targets.len(), "Ownership incomplete");
    let mut seen = HashSet::new();
    for file in owned {
        let Some(index) = targets.iter().position(|p| p == &file.path) else {
            anyhow::bail!("Unregistered file");
        };
        let payload = &game.files[index];
        ensure!(
            seen.insert(&file.path)
                && file.regular
                && !file.symlink_component
                && file.expected_sha256 == payload.sha256
                && file.observed_sha256 == payload.sha256
                && file.expected_bytes == payload.bytes
                && file.observed_bytes == payload.bytes,
            "Changed/unsafe ownership"
        );
    }
    Ok(targets)
}
#[derive(Debug)]
pub struct Candidate {
    pub id: String,
    pub game_path: PathBuf,
    pub bytes: u64,
    pub last_played: i64,
}
pub fn unload_candidates(
    mut candidates: Vec<Candidate>,
    resume: Option<&Path>,
    largest: bool,
) -> Vec<Candidate> {
    candidates.retain(|c| Some(c.game_path.as_path()) != resume);
    candidates.sort_by(|a, b| {
        if largest {
            b.bytes
                .cmp(&a.bytes)
                .then(a.last_played.cmp(&b.last_played))
                .then(a.id.cmp(&b.id))
        } else {
            a.last_played
                .cmp(&b.last_played)
                .then(b.bytes.cmp(&a.bytes))
                .then(a.id.cmp(&b.id))
        }
    });
    candidates
}
#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> Catalog {
        serde_json::from_str(r#"{"version":1,"library_id":"fixture","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","games":[{"id":"fixture","display_title":"Fake content","platform":"PS","bundle_revision":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","game_path":"Roms/PS/Fixture.bin","cache_path":"Roms/PS/Fixture.bin","install_kind":"file","launch_path":"Roms/PS/Fixture.bin","files":[{"id":"disc","relative_path":"","bytes":5,"sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"rom","http_route":"/v1/files/fixture/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/disc"}],"discs":[]}] }"#).unwrap()
    }
    #[test]
    fn network_catalog_paths_merge_and_guards() {
        let mut c = catalog();
        c.validate().unwrap();
        assert_eq!(c.merge_paths(Path::new("/fixture"), &[]).unwrap().len(), 1);
        let local = vec![PathBuf::from("/fixture/Roms/PS/Fixture.bin")];
        assert_eq!(
            c.merge_paths(Path::new("/fixture"), &local).unwrap().len(),
            1
        );
        for path in [
            "../Saves/a.bin",
            "Roms/PS/Imgs/a.bin",
            "Roms/PS/./a.bin",
            "Roms/PS/x.exe",
        ] {
            c.games[0].cache_path = path.into();
            assert!(c.validate().is_err());
        }
    }
    #[test]
    fn catalog_rejects_cross_game_identity_target_and_cache_root_overlap() {
        let mut c = catalog();
        let mut second = c.games[0].clone();
        second.id = "second".into();
        second.display_title = "Second".into();
        second.game_path = "Roms/PS/Fixture-2.bin".into();
        second.cache_path = second.game_path.clone();
        second.launch_path = second.game_path.clone();
        second.files[0].id = "second-disc".into();
        second.files[0].http_route = format!(
            "/v1/files/{}/{}/{}",
            second.id, second.bundle_revision, second.files[0].id
        );
        c.games.push(second.clone());
        c.validate().unwrap();

        c.games[1].game_path = "roms/ps/FIXTURE.BIN".into();
        c.games[1].cache_path = c.games[1].game_path.clone();
        c.games[1].launch_path = c.games[1].game_path.clone();
        assert!(c.validate().is_err());

        let mut c = catalog();
        c.games[0].install_kind = "directory".into();
        c.games[0].game_path = "Roms/PS/Alpha.m3u".into();
        c.games[0].cache_path = c.games[0].game_path.clone();
        c.games[0].launch_path = "Roms/PS/Alpha.m3u/Alpha.m3u".into();
        c.games[0].files[0].relative_path = "Alpha.m3u".into();
        c.games[0].files[0].role = "playlist".into();
        second.game_path = "Roms/PS/Alpha.m3u/nested/Second.cue".into();
        second.cache_path = "Roms/PS/Alpha.m3u/nested".into();
        second.launch_path = second.game_path.clone();
        second.install_kind = "directory".into();
        second.files[0].relative_path = "Second.cue".into();
        second.files[0].role = "cue".into();
        c.games.push(second.clone());
        assert!(c.validate().is_err());

        c.games[1].game_path = "Roms/PS/Alpha.m3u-other/Second.cue".into();
        c.games[1].cache_path = "Roms/PS/Alpha.m3u-other".into();
        c.games[1].launch_path = c.games[1].game_path.clone();
        c.validate().unwrap();
    }
    #[test]
    fn network_all_routes_require_full_verified_current_bundle() {
        let c = catalog();
        let g = &c.games[0];
        for route in [
            LaunchRoute::Play,
            LaunchRoute::ContextPlay,
            LaunchRoute::Reset,
            LaunchRoute::Resume,
        ] {
            assert!(!CacheState::Partial.permit_launch(g, route));
        }
        assert!(
            !CacheState::Ready {
                game_id: g.id.clone(),
                bundle_revision: g.bundle_revision.clone(),
                verified_files: 0
            }
            .permit_launch(g, LaunchRoute::Play)
        );
        assert!(
            CacheState::Ready {
                game_id: g.id.clone(),
                bundle_revision: g.bundle_revision.clone(),
                verified_files: 1
            }
            .permit_launch(g, LaunchRoute::Play)
        );
    }
    #[test]
    fn network_ready_record_is_bound_to_game_identity() {
        let c = catalog();
        let g = &c.games[0];
        let wrong = CacheState::Ready {
            game_id: "another-game".into(),
            bundle_revision: g.bundle_revision.clone(),
            verified_files: g.files.len(),
        };
        let ready = CacheState::Ready {
            game_id: g.id.clone(),
            bundle_revision: g.bundle_revision.clone(),
            verified_files: g.files.len(),
        };
        for route in [
            LaunchRoute::Play,
            LaunchRoute::ContextPlay,
            LaunchRoute::Reset,
            LaunchRoute::Resume,
        ] {
            assert!(!wrong.permit_launch(g, route));
            assert!(ready.permit_launch(g, route));
        }
    }
    #[test]
    fn network_space_and_unload_never_mutate() {
        let c = catalog();
        let g = &c.games[0];
        let plan = plan_space(g, 4096, &[], RESERVE_BYTES).unwrap();
        assert_eq!(plan.additional_bytes, 4096);
        assert_eq!(plan.shortage_bytes, 4096 + 65536);
        let partial = PartialAllocation {
            game_id: g.id.clone(),
            bundle_revision: g.bundle_revision.clone(),
            file_id: "disc".into(),
            sha256: g.files[0].sha256.clone(),
            allocated_bytes: 1,
        };
        assert!(plan_space(g, 4096, &[partial.clone()], u64::MAX).is_err());
        let mut valid_partial = partial;
        valid_partial.allocated_bytes = 4096;
        assert_eq!(
            plan_space(g, 4096, &[valid_partial.clone()], u64::MAX)
                .unwrap()
                .additional_bytes,
            0
        );
        valid_partial.bundle_revision = "old".into();
        assert!(plan_space(g, 4096, &[valid_partial], u64::MAX).is_err());
        let f = OwnedFile {
            path: Path::new("/fixture").join(&g.cache_path),
            expected_sha256: g.files[0].sha256.clone(),
            observed_sha256: g.files[0].sha256.clone(),
            expected_bytes: 5,
            observed_bytes: 5,
            regular: true,
            symlink_component: false,
        };
        let root = Path::new("/fixture");
        let ownership = OwnedCache {
            game_id: g.id.clone(),
            bundle_revision: g.bundle_revision.clone(),
            files: vec![f],
            unexpected_in_root: false,
        };
        assert_eq!(
            plan_unload(root, g, &ownership, None).unwrap(),
            vec![root.join(&g.cache_path)]
        );
        assert!(
            plan_unload(
                root,
                g,
                &ownership,
                Some(Path::new("/fixture/Roms/PS/Fixture.bin"))
            )
            .is_err()
        );
        let mut changed = ownership.clone();
        changed.files[0].observed_sha256 = "changed".into();
        assert!(plan_unload(root, g, &changed, None).is_err());
        let mut link = ownership.clone();
        link.files[0].symlink_component = true;
        assert!(plan_unload(root, g, &link, None).is_err());
        let mut unexpected = ownership.clone();
        unexpected.unexpected_in_root = true;
        assert!(plan_unload(root, g, &unexpected, None).is_err());
        let mut stale = ownership;
        stale.game_id = "another-game".into();
        assert!(plan_unload(root, g, &stale, None).is_err());
    }
    #[test]
    fn network_candidate_sorts_are_deterministic_and_resume_protected() {
        fn candidates() -> Vec<Candidate> {
            vec![
                Candidate {
                    id: "a".into(),
                    game_path: "/fixture/a".into(),
                    bytes: 1,
                    last_played: 0,
                },
                Candidate {
                    id: "b".into(),
                    game_path: "/fixture/b".into(),
                    bytes: 100,
                    last_played: 0,
                },
                Candidate {
                    id: "c".into(),
                    game_path: "/fixture/c".into(),
                    bytes: 200,
                    last_played: 10,
                },
                Candidate {
                    id: "protected".into(),
                    game_path: "/fixture/protected".into(),
                    bytes: 9999,
                    last_played: 0,
                },
            ]
        }
        let resume = Some(Path::new("/fixture/protected"));
        assert_eq!(
            unload_candidates(candidates(), resume, false)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a", "c"]
        );
        assert_eq!(
            unload_candidates(candidates(), resume, true)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "b", "a"]
        );
    }
}
