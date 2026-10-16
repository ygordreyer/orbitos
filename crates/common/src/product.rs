//! Product identity shared by launcher, menus and build artifacts.

pub const NAME: &str = "OrbitOS";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_ID: &str = env!("ORBITOS_BUILD_ID");
pub const UPDATE_CHANNEL: &str = "stable";
pub const UPSTREAM_NAME: &str = "Allium";
pub const UPSTREAM_ATTRIBUTION: &str = "Based on Allium by Wei Wen Goh and contributors";
pub const UPSTREAM_AUTHORS: &str = "Wei Wen Goh and contributors";
pub const UPSTREAM_SOURCE: &str = "github.com/goweiwen/Allium";
pub const LICENSE: &str = "MIT";
pub const LICENSE_NOTE: &str = "MIT License; provided without warranty";
pub const UPSTREAM_LICENSE: &str = include_str!("../../../LICENSE");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_identity_has_truthful_build_and_upstream_notice() {
        assert_eq!(NAME, "OrbitOS");
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!BUILD_ID.is_empty());
        assert!(BUILD_ID == "unknown" || BUILD_ID.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(UPDATE_CHANNEL, "stable");
        assert!(UPSTREAM_ATTRIBUTION.contains(UPSTREAM_NAME));
        assert!(UPSTREAM_ATTRIBUTION.contains(UPSTREAM_AUTHORS));
        assert!(UPSTREAM_LICENSE.contains("Copyright (c) 2025 Wei Wen Goh"));
        assert!(UPSTREAM_LICENSE.contains("WITHOUT WARRANTY"));
    }
}
