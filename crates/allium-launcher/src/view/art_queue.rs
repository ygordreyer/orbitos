use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use common::theme::pack::{Arrangement, Binding, Component, Kind};

use super::orbit::WarmArt;
use crate::entry::Entry;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct AssetKey(PathBuf, u8);

impl WarmArt {
    pub(super) fn entry(&self) -> &Entry {
        match self {
            Self::Metadata(e)
            | Self::Cover(e)
            | Self::CoverFallback(e)
            | Self::Hero(e)
            | Self::Logo(e)
            | Self::OtherCover(e) => e,
        }
    }
    pub(super) fn key(&self) -> AssetKey {
        AssetKey(
            self.entry().path().into(),
            match self {
                Self::Metadata(_) => 0,
                Self::Cover(_) => 1,
                Self::CoverFallback(_) => 2,
                Self::Hero(_) => 3,
                Self::Logo(_) => 4,
                Self::OtherCover(_) => 5,
            },
        )
    }
    pub(super) fn media_kinds(&self, tall: bool) -> Vec<common::media::Kind> {
        use common::media::Kind as M;
        match self {
            Self::Metadata(_) => vec![],
            Self::Cover(_) if tall => vec![M::TallCover],
            Self::Cover(_) => vec![M::Cover],
            Self::CoverFallback(_) => vec![M::PreparedCover],
            Self::Hero(_) => vec![M::Background],
            Self::Logo(_) => vec![M::Logo],
            Self::OtherCover(_) => vec![if tall { M::Cover } else { M::TallCover }],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Demand {
    pub hero: bool,
    pub logo: bool,
    pub cover: bool,
    pub rows: bool,
    pub next: bool,
    pub metadata: bool,
}
impl Demand {
    pub fn from_components<'a>(
        components: impl IntoIterator<Item = &'a Component>,
        ink: bool,
    ) -> Self {
        let mut demand = Self::default();
        for c in components {
            if c.layout == Some(Arrangement::Hidden) {
                continue;
            }
            if c.kind == Kind::Image {
                match c.binding {
                    Some(Binding::SelectedHero) => demand.hero = true,
                    Some(Binding::SelectedLogo) => demand.logo = true,
                    Some(Binding::SelectedCover) => demand.cover = true,
                    Some(Binding::NextCover) => demand.next = true,
                    _ => (),
                }
            }
            demand.logo |= c.logo;
            demand.metadata |= matches!(
                c.binding,
                Some(Binding::SelectedMetadata | Binding::SelectedBlurb)
            );
            if c.kind == Kind::Entries {
                demand.rows |= c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0;
                demand.next |= c.layout == Some(Arrangement::Shelf)
                    && (!ink || c.paint.as_ref().is_some_and(|p| p.peek.is_some()));
            }
        }
        demand
    }
}

pub(super) fn plan(
    entries: &[Entry],
    selected: usize,
    first: usize,
    count: usize,
    direction: isize,
    demand: Demand,
    neighbours: bool,
) -> VecDeque<WarmArt> {
    let mut work = VecDeque::new();
    let mut seen = HashSet::new();
    let mut push = |task: WarmArt| {
        if seen.insert(task.key()) {
            if common::perf_trace::recording() {
                use common::perf_trace::Count;
                let kind = match &task {
                    WarmArt::Hero(_) => Some(Count::ArtRequestHero),
                    WarmArt::Logo(_) => Some(Count::ArtRequestLogo),
                    WarmArt::Cover(entry) => Some(if entries.get(selected).is_some_and(|e| e.path() == entry.path()) {
                        Count::ArtRequestCover
                    } else if entries.get(first.saturating_add(count)).is_some_and(|e| e.path() == entry.path()) {
                        Count::ArtRequestNext
                    } else if entries.iter().skip(first).take(count).any(|e| e.path() == entry.path()) {
                        Count::ArtRequestEntries
                    } else {
                        Count::ArtRequestCover
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    common::perf_trace::count(kind, 1);
                }
            }
            work.push_back(task);
        }
    };
    let Some(current) = entries.get(selected) else {
        return work;
    };
    if demand.hero {
        push(WarmArt::Hero(current.clone()));
    }
    if demand.logo {
        push(WarmArt::Logo(current.clone()));
    }
    if demand.cover || demand.rows {
        push(WarmArt::Cover(current.clone()));
    }
    if demand.metadata && matches!(current, Entry::Game(_)) {
        push(WarmArt::Metadata(current.clone()));
    }
    let end = first.saturating_add(count.min(15)).min(entries.len());
    if demand.rows {
        if direction < 0 {
            for entry in entries.get(first..end).unwrap_or_default().iter().rev() {
                push(WarmArt::Cover(entry.clone()));
            }
        } else {
            for entry in entries.get(first..end).unwrap_or_default() {
                push(WarmArt::Cover(entry.clone()));
            }
        }
    }
    if demand.next {
        if let Some(entry) = entries.get(first.saturating_add(count)) {
            push(WarmArt::Cover(entry.clone()));
        }
    }
    if neighbours && entries.len() > 1 {
        let forward = if direction < 0 { entries.len() - 1 } else { 1 };
        for index in [
            (selected + forward) % entries.len(),
            (selected + entries.len() - forward) % entries.len(),
        ] {
            let e = &entries[index];
            if demand.hero {
                push(WarmArt::Hero(e.clone()));
            }
            if demand.logo {
                push(WarmArt::Logo(e.clone()));
            }
            if demand.cover || demand.rows {
                push(WarmArt::Cover(e.clone()));
            }
        }
    }
    work
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::theme::pack::{Pack, ScreenId};

    fn games() -> Vec<Entry> {
        (0..12)
            .map(|i| {
                Entry::Game(crate::entry::game::Game::new(
                    format!("/art-queue/G{i}.sfc").into(),
                ))
            })
            .collect()
    }
    fn load_pack(id: &str) -> Pack {
        if id == "compact" {
            return Pack::builtin(id).unwrap();
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../themes/{id}/theme.json"));
        Pack::parse(&std::fs::read(path).unwrap()).unwrap()
    }
    #[test]
    #[cfg(feature = "perf-bench")]
    fn hero_request_counters_follow_deduplicated_plan_and_reset_per_input() {
        let path = std::env::temp_dir().join(format!("allium-art-requests-{}.log", std::process::id()));
        common::perf_trace::start_probe(path.to_str().unwrap());
        { let _input = common::perf_trace::phase(common::perf_trace::Phase::Input); }
        let pack = load_pack("orbit-hero");
        let demand = Demand::from_components(&pack.screens[&ScreenId::Library].components, pack.ink());
        plan(&games(), 1, 0, 3, 1, demand, true);
        let measurement = common::perf_trace::probe_counts()["measurement"].clone();
        let names = measurement["counter_names"].as_array().unwrap();
        for (name, expected) in [("art_request_hero", 3), ("art_request_logo", 3),
            ("art_request_cover", 1), ("art_request_next", 1), ("art_request_entries", 2)] {
            let index = names.iter().position(|n| n == name).unwrap();
            assert_eq!(measurement["input"]["counters"][index], expected, "{name}");
        }
        { let _input = common::perf_trace::phase(common::perf_trace::Phase::Input); }
        assert!(common::perf_trace::probe_counts()["measurement"]["input"]["counters"]
            .as_array().unwrap().iter().all(|n| n == 0));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn actual_screens_request_only_drawn_asset_kinds() {
        let entries = games();
        for id in ["orbit-hero", "compact"] {
            let pack = load_pack(id);
            let demand =
                Demand::from_components(&pack.screens[&ScreenId::Recents].components, pack.ink());
            let work = plan(&entries, 1, 0, 3, 1, demand, true);
            assert!(work.is_empty(), "{id} recents: {work:?}");
        }
        let pack = load_pack("compact-plus");
        for screen in [ScreenId::Library, ScreenId::Recents] {
            let demand = Demand::from_components(&pack.screens[&screen].components, pack.ink());
            assert!(!demand.rows && !demand.next);
            let work = plan(&entries, 4, 0, 9, 1, demand, true);
            assert!(work.iter().all(|t| {
                [3, 4, 5]
                    .iter()
                    .any(|i| t.entry().path() == entries[*i].path())
            }));
        }
        let pack = load_pack("big-picture");
        let demand =
            Demand::from_components(&pack.screens[&ScreenId::Recents].components, pack.ink());
        assert!(!demand.hero);
        assert!(
            plan(&entries, 1, 0, 5, 1, demand, true)
                .iter()
                .all(|t| !matches!(t, WarmArt::Hero(_)))
        );
    }
    #[test]
    fn selected_hero_then_viewport_then_directional_and_opposite_neighbors() {
        let entries = games();
        let work = plan(
            &entries,
            4,
            4,
            3,
            1,
            Demand {
                hero: true,
                rows: true,
                ..Demand::default()
            },
            true,
        );
        let actual: Vec<_> = work
            .iter()
            .map(|t| (t.key().1, t.entry().path().to_owned()))
            .collect();
        assert_eq!(
            actual,
            [(3, 4), (1, 4), (1, 5), (1, 6), (3, 5), (3, 3), (1, 3)]
                .map(|(kind, i)| (kind, entries[i].path().to_owned()))
        );
        let reverse = plan(
            &entries,
            4,
            2,
            3,
            -1,
            Demand {
                hero: true,
                rows: true,
                ..Demand::default()
            },
            true,
        );
        let heroes: Vec<_> = reverse
            .iter()
            .filter(|t| matches!(t, WarmArt::Hero(_)))
            .map(|t| t.entry().path())
            .collect();
        assert_eq!(
            heroes,
            [entries[4].path(), entries[3].path(), entries[5].path()]
        );
    }
    #[test]
    fn duplicates_wraparound_and_empty_demand() {
        let entries = games();
        for entries in [&entries[..1], &entries[..2], &entries[..]] {
            let work = plan(
                entries,
                0,
                0,
                3,
                1,
                Demand {
                    hero: true,
                    logo: true,
                    cover: true,
                    rows: true,
                    next: true,
                    metadata: true,
                },
                true,
            );
            assert_eq!(
                work.len(),
                work.iter().map(WarmArt::key).collect::<HashSet<_>>().len()
            );
            assert_eq!(
                work.iter()
                    .filter(|t| matches!(t, WarmArt::Metadata(_)))
                    .count(),
                1
            );
            assert!(plan(entries, 0, 0, 3, 1, Demand::default(), true).is_empty());
        }
        assert!(plan(&[], 0, 0, 3, 1, Demand::default(), true).is_empty());
    }
    #[test]
    fn tasks_resolve_only_their_requested_kinds() {
        use common::media::Kind as M;
        let entry = games().remove(0);
        assert_eq!(
            WarmArt::Hero(entry.clone()).media_kinds(false),
            [M::Background]
        );
        assert_eq!(WarmArt::Logo(entry.clone()).media_kinds(true), [M::Logo]);
        assert_eq!(WarmArt::Cover(entry.clone()).media_kinds(false), [M::Cover]);
        assert_eq!(
            WarmArt::Cover(entry.clone()).media_kinds(true),
            [M::TallCover]
        );
        assert!(WarmArt::Metadata(entry).media_kinds(false).is_empty());
    }
}
