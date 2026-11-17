use crate::models::ReaderSettings;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

impl Theme {
    pub fn index(self) -> i32 {
        match self {
            Theme::Light => 0,
            Theme::Dark => 1,
            Theme::System => 2,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Theme::Light,
            1 => Theme::Dark,
            _ => Theme::System,
        }
    }
}

/// Global application settings (stored as JSON in the database).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub reader: ReaderSettings,
    pub show_page_number: bool,
    /// Empty = default location inside the data directory.
    pub download_dir: String,
    pub max_concurrent_downloads: u32,
    pub cache_limit_mb: u64,
    pub update_on_startup: bool,
    pub auto_download_new: bool,
    pub library_sort: i32,
    pub library_sort_desc: bool,
    pub chapter_sort_desc: bool,
    pub hide_read_chapters: bool,
    pub mangadex_language: String,
    pub mangadex_nsfw: bool,
    pub local_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            reader: ReaderSettings::default(),
            show_page_number: true,
            download_dir: String::new(),
            max_concurrent_downloads: 2,
            cache_limit_mb: 256,
            update_on_startup: false,
            auto_download_new: false,
            library_sort: 0,
            library_sort_desc: false,
            chapter_sort_desc: true,
            hide_read_chapters: false,
            mangadex_language: "en".into(),
            mangadex_nsfw: false,
            local_dir: String::new(),
        }
    }
}
