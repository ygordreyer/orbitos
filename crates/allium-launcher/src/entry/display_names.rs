use std::collections::HashMap;
use std::path::PathBuf;

use lazy_static::lazy_static;
use regex::Regex;

use super::Entry;

#[derive(Debug, Default, Clone)]
pub(crate) struct DisplayNames(HashMap<PathBuf, (String, String)>);

impl DisplayNames {
    pub(crate) fn new(entries: &[Entry]) -> Self {
        lazy_static! {
            static ref PARENS: Regex = Regex::new(r"\([^()]*\)").unwrap();
            static ref BRACKETS: Regex = Regex::new(r"\[[^\[\]]*\]").unwrap();
        }
        let mut groups: HashMap<String, Vec<&Entry>> = HashMap::new();
        for entry in entries {
            if matches!(entry, Entry::Game(_)) {
                groups.entry(key(entry.name())).or_default().push(entry);
            }
        }
        let mut names = HashMap::new();
        for games in groups.values().filter(|games| games.len() > 1) {
            let mut suffixes = vec![String::new(); games.len()];
            for pattern in [&*PARENS, &*BRACKETS] {
                let parts: Vec<Vec<String>> = games.iter().map(|entry| {
                    let stem = entry.path().file_stem().unwrap_or_default().to_string_lossy();
                    pattern.find_iter(&stem).map(|part| part.as_str().to_owned()).collect()
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
            for (entry, mut suffix) in games.iter().zip(suffixes) {
                // An absent revision has no differing group to display. Keep its region as context.
                if suffix.is_empty() {
                    let stem = entry.path().file_stem().unwrap_or_default().to_string_lossy();
                    suffix = PARENS.find_iter(&stem).map(|part| part.as_str()).collect::<Vec<_>>().join(" ");
                    if suffix.is_empty() {
                        let extension = entry.path().extension().unwrap_or_default().to_string_lossy().to_uppercase();
                        if !extension.is_empty() {
                            suffix = format!("({extension})");
                        }
                    }
                }
                if !suffix.is_empty() {
                    names.insert(entry.path().to_path_buf(), (entry.name().to_owned(), format!("{} {suffix}", entry.name().trim())));
                }
            }
        }
        Self(names)
    }

    pub(crate) fn name<'a>(&'a self, entry: &'a Entry) -> &'a str {
        self.0.get(entry.path()).filter(|(original, _)| original == entry.name())
            .map(|(_, name)| name.as_str()).unwrap_or_else(|| entry.name())
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
