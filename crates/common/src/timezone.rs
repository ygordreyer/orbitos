//! Saved POSIX timezone identifiers shared by settings and process startup.
use std::path::Path;

pub const TIMEZONE_VALUES: [&str; 39] = [
    "UTC+12",
    "UTC+11",
    "UTC+10",
    "UTC+9:30",
    "UTC+9",
    "UTC+8",
    "UTC+7",
    "UTC+6",
    "UTC+5",
    "UTC+4",
    "UTC+3:30",
    "UTC+3",
    "UTC+2",
    "UTC+1",
    "UTC-0",
    "UTC-1",
    "UTC-2",
    "UTC-3",
    "UTC-3:30",
    "UTC-4",
    "UTC-4:30",
    "UTC-5",
    "UTC-5:30",
    "UTC-5:45",
    "UTC-6",
    "UTC-6:30",
    "UTC-7",
    "UTC-8",
    "UTC-8:45",
    "UTC-9",
    "UTC-9:30",
    "UTC-10",
    "UTC-10:30",
    "UTC-11",
    "UTC-12",
    "UTC-12:45",
    "UTC-13",
    "UTC-13:45",
    "UTC-14",
];
pub const TIMEZONE_NAMES: [&str; 39] = [
    "UTC-12",
    "UTC-11",
    "UTC-10",
    "UTC-9:30",
    "UTC-9",
    "UTC-8",
    "UTC-7",
    "UTC-6",
    "UTC-5",
    "UTC-4",
    "UTC-3:30",
    "UTC-3",
    "UTC-2",
    "UTC-1",
    "UTC+0",
    "UTC+1",
    "UTC+2",
    "UTC+3",
    "UTC+3:30",
    "UTC+4",
    "UTC+4:30",
    "UTC+5",
    "UTC+5:30",
    "UTC+5:45",
    "UTC+6",
    "UTC+6:30",
    "UTC+7",
    "UTC+8",
    "UTC+8:45",
    "UTC+9",
    "UTC+9:30",
    "UTC+10",
    "UTC+10:30",
    "UTC+11",
    "UTC+12",
    "UTC+12:45",
    "UTC+13",
    "UTC+13:45",
    "UTC+14",
];

fn offset_minutes(value: &str) -> Option<i32> {
    if !TIMEZONE_VALUES.contains(&value) { return None; }
    let (hours, minutes) = value[4..].split_once(':').unwrap_or((&value[4..], "0"));
    let offset = hours.parse::<i32>().ok()? * 60 + minutes.parse::<i32>().ok()?;
    Some(if value.as_bytes()[3] == b'+' { -offset } else { offset })
}

/// Apply only known zones. Missing or corrupt state must preserve the inherited TZ.
pub fn load_saved_timezone() -> bool {
    load_timezone_from(&crate::constants::ALLIUM_TIMEZONE)
}

fn load_timezone_from(path: &Path) -> bool {
    let Ok(value) = std::fs::read_to_string(path) else { return false; };
    let value = value.trim();
    if offset_minutes(value).is_none() { return false; }
    unsafe { std::env::set_var("TZ", value); }
    true
}

/// Avoid chrono's one-second local-zone cache after a settings change.
pub fn local_now() -> chrono::DateTime<chrono::FixedOffset> {
    if let Some(minutes) = std::env::var("TZ").ok().as_deref().and_then(offset_minutes) {
        return chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(minutes * 60).unwrap());
    }
    chrono::Local::now().fixed_offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timezone_arrays_are_monotonic_and_paired() {
        assert_eq!(TIMEZONE_VALUES.len(), TIMEZONE_NAMES.len());
        let offsets: Vec<_> = TIMEZONE_VALUES.iter().map(|value| offset_minutes(value).unwrap()).collect();
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        for (value, name) in TIMEZONE_VALUES.iter().zip(TIMEZONE_NAMES) {
            assert_eq!(&value[4..], &name[4..]);
            assert_ne!(value.as_bytes()[3], name.as_bytes()[3]);
        }
    }

    #[test]
    fn saved_timezone_valid_missing_and_junk() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) { unsafe {
                match &self.0 { Some(value) => std::env::set_var("TZ", value), None => std::env::remove_var("TZ") }
            } }
        }
        let _restore = Restore(std::env::var_os("TZ"));
        let path = std::env::temp_dir().join(format!("orbit-timezone-{}", std::process::id()));
        unsafe { std::env::set_var("TZ", "UTC-0"); }
        assert!(!load_timezone_from(&path));
        assert_eq!(std::env::var("TZ").unwrap(), "UTC-0");
        std::fs::write(&path, "UTC+3\n").unwrap();
        assert!(load_timezone_from(&path));
        assert_eq!(std::env::var("TZ").unwrap(), "UTC+3");
        assert_eq!(local_now().offset().local_minus_utc(), -3 * 3600);
        std::fs::write(&path, "junk").unwrap();
        assert!(!load_timezone_from(&path));
        assert_eq!(std::env::var("TZ").unwrap(), "UTC+3");
        std::fs::remove_file(path).unwrap();
    }
}
