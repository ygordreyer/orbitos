use std::collections::HashMap;
use std::path::PathBuf;

use lazy_static::lazy_static;
use regex::Regex;

use super::Entry;

#[derive(Debug, Default, Clone)]
pub(crate) struct DisplayNames(HashMap<PathBuf, (String, String, usize)>);

impl DisplayNames {
    pub(crate) fn new(entries: &[Entry]) -> Self {
        lazy_static! {
            static ref PARENS: Regex = Regex::new(r"\([^()]*\)").unwrap();
            static ref BRACKETS: Regex = Regex::new(r"\[[^\[\]]*\]").unwrap();
        }
        let game_paths = entries.iter().filter_map(|entry| match entry {
            Entry::Game(game) => Some(game.path.clone()),
            _ => None,
        }).collect();
        let mut groups: HashMap<String, HashMap<PathBuf, Vec<&Entry>>> = HashMap::new();
        for entry in entries {
            if let Entry::Game(game) = entry {
                groups.entry(key(entry.name())).or_default()
                    .entry(game.display_identity_in(&game_paths)).or_default().push(entry);
            }
        }
        let mut names = HashMap::new();
        for aliases in groups.values().filter(|games| games.len() > 1) {
            let identities: Vec<_> = aliases.iter().collect();
            let games: Vec<_> = identities.iter().map(|(identity, entries)| {
                entries.iter().find(|entry| entry.path() == identity.as_path()).unwrap_or(&entries[0])
            }).copied().collect();
            let stems: Vec<_> = identities.iter().map(|(path, entries)| {
                // Folder identities keep dotted tags such as "(v1.1)": only a plain extension is stripped.
                let plain_extension = path.extension().map(|extension| extension.to_string_lossy())
                    .is_some_and(|extension| extension.len() <= 5 && extension.bytes().all(|b| b.is_ascii_alphanumeric()));
                let name = if !plain_extension || entries.iter().any(|entry| entry.path() != path.as_path()) {
                    path.file_name()
                } else {
                    path.file_stem()
                };
                name.unwrap_or_default().to_string_lossy()
            }).collect();
            let mut suffixes = vec![String::new(); games.len()];
            for pattern in [&*PARENS, &*BRACKETS] {
                let parts: Vec<Vec<String>> = stems.iter().map(|stem| {
                    pattern.find_iter(stem).map(|part| part.as_str().to_owned()).collect()
                }).collect();
                for indices in collisions(&suffixes) {
                    let width = indices.iter().map(|&i| parts[i].len()).max().unwrap_or(0);
                    for &i in &indices {
                        let mut columns: Vec<usize> = (0..width).filter(|&column| {
                            indices.iter().any(|&j| part_key(&parts[i], column) != part_key(&parts[j], column))
                        }).collect();
                        // Drop redundant groups while preserving this filename's distinctions.
                        for column in columns.clone().into_iter().rev() {
                            if indices.iter().all(|&j| {
                                parts[i] == parts[j] || columns.iter().any(|&other| {
                                    other != column && part_key(&parts[i], other) != part_key(&parts[j], other)
                                })
                            }) {
                                columns.retain(|&other| other != column);
                            }
                        }
                        for column in columns {
                            if let Some(part) = parts[i].get(column) {
                                append(&mut suffixes[i], part);
                            }
                        }
                    }
                }
            }
            for indices in collisions(&suffixes) {
                for i in indices {
                    let extension = games[i].path().extension().unwrap_or_default().to_string_lossy().to_uppercase();
                    if !extension.is_empty() {
                        append(&mut suffixes[i], &format!("({extension})"));
                    }
                }
            }
            for (i, (entry, mut suffix)) in games.iter().zip(suffixes).enumerate() {
                // An absent revision has no differing group to display. Keep its region as context.
                if suffix.is_empty() {
                    suffix = PARENS.find_iter(&stems[i]).map(|part| part.as_str()).collect::<Vec<_>>().join(" ");
                    if suffix.is_empty() {
                        let extension = entry.path().extension().unwrap_or_default().to_string_lossy().to_uppercase();
                        if !extension.is_empty() {
                            suffix = format!("({extension})");
                        }
                    }
                }
                if !suffix.is_empty() {
                    for alias in identities[i].1 {
                        names.insert(alias.path().to_path_buf(), (alias.name().to_owned(), format!("{} {suffix}", alias.name().trim()), suffix.len() + 1));
                    }
                }
            }
        }
        Self(names)
    }

    pub(crate) fn suffix_len(&self, entry: &Entry) -> usize {
        self.0.get(entry.path()).filter(|(original, _, _)| original == entry.name())
            .map_or(0, |(_, _, len)| *len)
    }

    pub(crate) fn name<'a>(&'a self, entry: &'a Entry) -> &'a str {
        self.0.get(entry.path()).filter(|(original, _, _)| original == entry.name())
            .map(|(_, name, _)| name.as_str()).unwrap_or_else(|| entry.name())
    }
}

fn key(name: &str) -> String {
    name.trim().to_lowercase()
}

fn part_key(parts: &[String], column: usize) -> String {
    parts.get(column).map(|part| key(part)).unwrap_or_default()
}

fn collisions(suffixes: &[String]) -> Vec<Vec<usize>> {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, suffix) in suffixes.iter().enumerate() {
        groups.entry(key(suffix)).or_default().push(i);
    }
    groups.into_values().filter(|indices| indices.len() > 1).collect()
}

fn append(suffix: &mut String, part: &str) {
    if !suffix.is_empty() {
        suffix.push(' ');
    }
    suffix.push_str(part);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::game::Game;

    fn entries(files: &[&str], names: &[&str]) -> Vec<Entry> {
        files.iter().zip(names).map(|(file, name)| {
            let mut game = Game::new((*file).into());
            game.name = (*name).into();
            Entry::Game(game)
        }).collect()
    }

    fn labels(entries: &[Entry]) -> Vec<String> {
        let display = DisplayNames::new(entries);
        entries.iter().map(|entry| display.name(entry).to_owned()).collect()
    }

    #[test]
    fn fix20_aliases_share_one_collision_identity_and_real_regions_still_differ() {
        let folder = "PS/Kowai (Japan)";
        let cue = "PS/Kowai (Japan)/Kowai (Japan).cue";
        let games = entries(&[folder, cue], &["Kowai", "Kowai"]);
        assert_eq!(labels(&games), ["Kowai", "Kowai"]);
        let games = entries(&[folder, cue, "PS/Kowai (Europe).chd"], &["Kowai", "Kowai", "Kowai"]);
        assert_eq!(labels(&games), ["Kowai (Japan)", "Kowai (Japan)", "Kowai (Europe)"]);
        common::media::set_game_containers([("PS/Kowai (Japan)/disc.cue".into(), folder.into())]);
        let games = entries(&[folder, "PS/Kowai (Japan)/disc.cue"], &["Kowai", "Kowai"]);
        crate::entry::load_work::take();
        assert_eq!(labels(&games), ["Kowai", "Kowai"]);
        assert_eq!(crate::entry::load_work::take(), Default::default());
        let games = entries(&["PS/Kowai (Japan)/disc.cue", "PS/Kowai (Europe).chd"], &["Kowai", "Kowai"]);
        assert_eq!(labels(&games), ["Kowai (Japan)", "Kowai (Europe)"]);
        common::media::set_game_containers(std::iter::empty());
    }

    #[test]
    fn fix20b_list_folder_and_ancestor_container_share_collision_identity() {
        let folder = "fix20b-display/Folder Title (Japan)";
        let launch = "fix20b-display/Folder Title (Japan)/disc.cue";
        let games = entries(&[folder, launch], &["Folder Title", "Folder Title"]);
        crate::entry::load_work::take();
        assert_eq!(labels(&games), ["Folder Title", "Folder Title"]);
        assert_eq!(crate::entry::load_work::take(), Default::default());
        let nested = "fix20b-display/Folder Title (Japan)/nested/disc.cue";
        common::media::set_game_containers([(nested.into(), folder.into())]);
        let games = entries(&[nested, "fix20b-display/Folder Title (Europe).chd"], &["Folder Title", "Folder Title"]);
        assert_eq!(labels(&games), ["Folder Title (Japan)", "Folder Title (Europe)"]);
        common::media::set_game_containers(std::iter::empty());
        let games = entries(&["PS/Multiple/Disc (Japan).chd", "PS/Multiple/Disc (Europe).chd"], &["Disc", "Disc"]);
        assert_eq!(labels(&games), ["Disc (Japan)", "Disc (Europe)"]);
    }

    #[test]
    fn fix23_suffix_metadata_keeps_long_region_twins_distinct() {
        let title = "Radical Rex: Shred Pre-historic pavement";
        let games = entries(&["Radical Rex (USA).sfc", "Radical Rex (Europe).zip"], &[title, title]);
        let names = DisplayNames::new(&games);
        let fitted: Vec<_> = games.iter().map(|entry| {
            let value = names.name(entry);
            common::display::font::fit_suffix(value, names.suffix_len(entry), 30.0, "...",
                |text| text.chars().count() as f32,
                |text| vec![common::display::font::ellipsize(text, 30, |text| text.chars().count() as u32)])[0].clone()
        }).collect();
        assert_eq!(fitted, ["Radical Rex: Shred Pr... (USA)", "Radical Rex: Shred... (Europe)"]);
        assert!(fitted.iter().all(|text| text.chars().count() <= 30));
        let games = entries(&["Unique (USA).sfc"], &[title]);
        assert_eq!(DisplayNames::new(&games).suffix_len(&games[0]), 0);
    }

    #[test]
    fn region_collision() {
        let games = entries(&["Blob (USA).nes", "Blob (Europe).nes"], &["Blob", "Blob"]);
        assert_eq!(labels(&games), ["Blob (USA)", "Blob (Europe)"]);
    }

    #[test]
    fn revision_collision_omits_shared_region() {
        let games = entries(&["Island (USA) (Rev 1).nes", "Island (USA) (Rev 2).nes"], &["Island", "Island"]);
        assert_eq!(labels(&games), ["Island (Rev 1)", "Island (Rev 2)"]);
    }

    #[test]
    fn dotted_container_revisions_remain_distinct_with_folder_aliases() {
        let files = ["PS/Game (Rev 1.1)/Game (Rev 1.1).cue", "PS/Game (Rev 1.2)/Game (Rev 1.2).cue"];
        let games = entries(&files, &["Game", "Game"]);
        assert_eq!(labels(&games), ["Game (Rev 1.1)", "Game (Rev 1.2)"]);
        let games = entries(&["PS/Game (Rev 1.1)", files[0], "PS/Game (Rev 1.2)", files[1]],
            &["Game", "Game", "Game", "Game"]);
        assert_eq!(labels(&games), ["Game (Rev 1.1)", "Game (Rev 1.1)", "Game (Rev 1.2)", "Game (Rev 1.2)"]);
        let games = entries(&["PS/Game (Rev 1.1).cue", "PS/Game (Rev 1.2).cue"], &["Game", "Game"]);
        assert_eq!(labels(&games), ["Game (Rev 1.1)", "Game (Rev 1.2)"]);
        let games = entries(&["PS/Game (Europe) (v1.0)", "PS/Game (Europe) (v1.1)"], &["Game", "Game"]);
        assert_eq!(labels(&games), ["Game (v1.0)", "Game (v1.1)"]);
    }

    #[test]
    fn extension_only_collision() {
        let games = entries(&["Goemon (Japan) [T-En v1.01] [n].nes", "Goemon (Japan) [T-En v1.01] [n].zip"], &["Goemon", "Goemon"]);
        assert_eq!(labels(&games), ["Goemon (NES)", "Goemon (ZIP)"]);
    }

    #[test]
    fn three_way_collision_refines_only_remaining_duplicates() {
        let games = entries(&["Blob (USA).nes", "Blob (USA).zip", "Blob (Europe).nes"], &["Blob", "Blob", "Blob"]);
        assert_eq!(labels(&games), ["Blob (USA) (NES)", "Blob (USA) (ZIP)", "Blob (Europe)"]);
    }

    #[test]
    fn bracket_collision_omits_shared_tags() {
        let games = entries(&["Goemon (Japan) [T-En v1] [n].nes", "Goemon (Japan) [T-En v2] [n].nes"], &["Goemon", "Goemon"]);
        assert_eq!(labels(&games), ["Goemon [T-En v1]", "Goemon [T-En v2]"]);
    }

    #[test]
    fn collisions_are_case_insensitive_and_trimmed_without_mutating_entries() {
        let games = entries(&["Island (USA).nes", "Island (Europe).nes"], &[" Island ", "island"]);
        let original = games.clone();
        assert_eq!(labels(&games), ["Island (USA)", "island (Europe)"]);
        assert_eq!(games, original);
    }

    #[test]
    fn non_colliding_names_are_untouched() {
        let games = entries(&["Blob (USA).nes", "Island (USA).zip"], &[" Blob ", "Island"]);
        assert_eq!(labels(&games), [" Blob ", "Island"]);
    }

    #[test]
    fn shared_revision_is_omitted_when_region_already_distinguishes() {
        let games = entries(&["Blob (USA) (Rev 1).nes", "Blob (Europe) (Rev 2).nes"], &["Blob", "Blob"]);
        assert_eq!(labels(&games), ["Blob (USA)", "Blob (Europe)"]);
    }

    #[test]
    fn missing_revision_keeps_region_as_context() {
        let games = entries(&["Blob (USA).nes", "Blob (USA) (Rev 1).nes"], &["Blob", "Blob"]);
        assert_eq!(labels(&games), ["Blob (USA)", "Blob (Rev 1)"]);
    }

    #[test]
    fn rebuilding_after_metadata_changes_removes_obsolete_suffixes() {
        let mut games = entries(&["Blob (USA).nes", "Blob (Europe).nes"], &["Blob", "Blob"]);
        assert_eq!(labels(&games), ["Blob (USA)", "Blob (Europe)"]);
        let Entry::Game(game) = &mut games[1] else { unreachable!() };
        game.name = "Different".into();
        assert_eq!(labels(&games), ["Blob", "Different"]);
    }

    #[test]
    fn cached_label_does_not_hide_a_new_name() {
        let mut games = entries(&["Blob (USA).nes", "Blob (Europe).nes"], &["Blob", "Blob"]);
        let display = DisplayNames::new(&games);
        let Entry::Game(game) = &mut games[0] else { unreachable!() };
        game.name = "Renamed".into();
        assert_eq!(display.name(&games[0]), "Renamed");
    }
}
