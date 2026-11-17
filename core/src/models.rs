use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Status {
    #[default]
    Unknown,
    Ongoing,
    Completed,
    Hiatus,
    Cancelled,
}

impl Status {
    pub fn as_i32(self) -> i32 {
        match self {
            Status::Unknown => 0,
            Status::Ongoing => 1,
            Status::Completed => 2,
            Status::Hiatus => 3,
            Status::Cancelled => 4,
        }
    }
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Status::Ongoing,
            2 => Status::Completed,
            3 => Status::Hiatus,
            4 => Status::Cancelled,
            _ => Status::Unknown,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Unknown => "Unknown",
            Status::Ongoing => "Ongoing",
            Status::Completed => "Completed",
            Status::Hiatus => "On hiatus",
            Status::Cancelled => "Cancelled",
        }
    }
}

/// Reader mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReaderMode {
    #[default]
    RightToLeft,
    LeftToRight,
    Vertical,
    Webtoon,
}

impl ReaderMode {
    pub fn index(self) -> i32 {
        match self {
            ReaderMode::RightToLeft => 0,
            ReaderMode::LeftToRight => 1,
            ReaderMode::Vertical => 2,
            ReaderMode::Webtoon => 3,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => ReaderMode::LeftToRight,
            2 => ReaderMode::Vertical,
            3 => ReaderMode::Webtoon,
            _ => ReaderMode::RightToLeft,
        }
    }
}

/// How pages are scaled to the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Scaling {
    FitWidth,
    FitHeight,
    Original,
    #[default]
    Auto,
}

impl Scaling {
    pub fn index(self) -> i32 {
        match self {
            Scaling::FitWidth => 0,
            Scaling::FitHeight => 1,
            Scaling::Original => 2,
            Scaling::Auto => 3,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Scaling::FitWidth,
            1 => Scaling::FitHeight,
            2 => Scaling::Original,
            _ => Scaling::Auto,
        }
    }
}

/// Reader options; stored globally and optionally overridden per manga.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReaderSettings {
    pub mode: ReaderMode,
    pub scaling: Scaling,
    pub double_page: bool,
    pub crop_borders: bool,
    /// 0 black, 1 dark gray, 2 white, 3 sepia
    pub background: i32,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            mode: ReaderMode::RightToLeft,
            scaling: Scaling::Auto,
            double_page: false,
            crop_borders: false,
            background: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manga {
    pub id: i64,
    pub source: String,
    pub key: String,
    pub title: String,
    pub cover_url: String,
    pub description: String,
    pub authors: String,
    pub genres: Vec<String>,
    pub status: Status,
    pub in_library: bool,
    pub added_at: i64,
    pub updated_at: i64,
    /// Per-manga reader settings override.
    pub reader_override: Option<ReaderSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub id: i64,
    pub manga_id: i64,
    pub key: String,
    pub title: String,
    pub number: Option<f64>,
    pub volume: Option<String>,
    pub scanlator: String,
    pub published: i64,
    pub read: bool,
    pub last_page: i32,
    pub page_count: i32,
    pub downloaded: bool,
    pub is_new: bool,
    pub found_at: i64,
    pub last_read_at: i64,
    /// Order in which the source returned the chapter (0 = newest).
    pub position: i32,
}

impl Chapter {
    pub fn display_title(&self) -> String {
        let mut s = String::new();
        if let Some(v) = &self.volume {
            if !v.is_empty() {
                s.push_str(&format!("Vol. {v} "));
            }
        }
        if let Some(n) = self.number {
            s.push_str(&format!("Ch. {} ", crate::util::fmt_number(n)));
        }
        let t = self.title.trim();
        if !t.is_empty() {
            if !s.is_empty() {
                s.push_str("— ");
            }
            s.push_str(t);
        }
        if s.trim().is_empty() {
            "Chapter".into()
        } else {
            s.trim().to_string()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub position: i32,
}

/// Manga row in the library with aggregate counts.
#[derive(Debug, Clone)]
pub struct LibraryEntry {
    pub manga: Manga,
    pub unread: i64,
    pub total: i64,
    pub new_count: i64,
    pub downloaded: i64,
    pub last_read_at: i64,
    pub category_ids: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub chapter: Chapter,
    pub manga: Manga,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadState {
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
}

impl DownloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            DownloadState::Queued => "queued",
            DownloadState::Downloading => "downloading",
            DownloadState::Paused => "paused",
            DownloadState::Done => "done",
            DownloadState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "downloading" => DownloadState::Downloading,
            "paused" => DownloadState::Paused,
            "done" => DownloadState::Done,
            "failed" => DownloadState::Failed,
            _ => DownloadState::Queued,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadJob {
    pub chapter_id: i64,
    pub manga_id: i64,
    pub state: DownloadState,
    pub done_pages: i32,
    pub total_pages: i32,
    pub error: String,
    pub manga_title: String,
    pub chapter_title: String,
}

/// Library sort options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibrarySort {
    #[default]
    Title,
    LastRead,
    LastUpdated,
    DateAdded,
    Unread,
}

impl LibrarySort {
    pub fn index(self) -> i32 {
        match self {
            LibrarySort::Title => 0,
            LibrarySort::LastRead => 1,
            LibrarySort::LastUpdated => 2,
            LibrarySort::DateAdded => 3,
            LibrarySort::Unread => 4,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => LibrarySort::LastRead,
            2 => LibrarySort::LastUpdated,
            3 => LibrarySort::DateAdded,
            4 => LibrarySort::Unread,
            _ => LibrarySort::Title,
        }
    }
}
