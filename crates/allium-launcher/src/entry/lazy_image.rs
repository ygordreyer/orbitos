use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LazyImage {
    /// Path to the file
    Unknown(PathBuf),
    /// Path to the found image
    Found(PathBuf),
    NotFound,
}

impl LazyImage {
    pub fn from_path(path: &Path, image: Option<PathBuf>) -> Self {
        match image {
            Some(image) => Self::Found(image),
            _ => Self::Unknown(path.to_path_buf()),
        }
    }

    /// Searches for the image path, caches it, and returns it
    pub fn image(&mut self) -> Option<&Path> {
        let path = match self {
            Self::Unknown(path) => path,
            Self::Found(path) => return Some(path.as_path()),
            Self::NotFound => return None,
        };

        let image = common::media::resolve(path, common::media::Kind::Cover);

        *self = match image {
            Some(image) => Self::Found(image),
            None => Self::NotFound,
        };

        match self {
            Self::Found(path) => Some(path.as_path()),
            _ => None,
        }
    }

    pub fn try_image(&self) -> Option<&Path> {
        match self {
            Self::Found(path) => Some(path.as_path()),
            _ => None,
        }
    }
}

impl From<PathBuf> for LazyImage {
    fn from(path: PathBuf) -> Self {
        Self::Found(path)
    }
}
