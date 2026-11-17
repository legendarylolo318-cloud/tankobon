//! Local source: folders and CBZ/CBR/CB7 archives under a root directory.
//!
//! Layout understood:
//! * `root/Series/Chapter 1/001.jpg ...`  – folder series with folder chapters
//! * `root/Series/Chapter 1.cbz`          – folder series with archive chapters
//! * `root/Series.cbz`                    – a single archive is a one-chapter series
//! * `root/Series/001.jpg ...`            – a folder of images is a one-chapter series

use super::{ChapterEntry, MangaDetails, MangaEntry, Paged, Source};
use crate::archive::{is_archive, is_image_name, ArchiveKind, ArchiveReader};
use crate::models::Status;
use crate::util::{natural_cmp, stable_hash};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;
use std::time::UNIX_EPOCH;

const PAGE_SIZE: usize = 48;
const COVER_PREFIX: &str = "local-cover:";
const FILE_PREFIX: &str = "file:";

pub struct Local {
    root: RwLock<PathBuf>,
    cache: PathBuf,
}

impl Local {
    pub fn new(root: PathBuf, cache: PathBuf) -> Self {
        Self { root: RwLock::new(root), cache }
    }

    fn root(&self) -> PathBuf {
        self.root.read().map(|r| r.clone()).unwrap_or_default()
    }

    fn resolve(&self, key: &str) -> Result<PathBuf> {
        let rel = Path::new(key);
        if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
            bail!("invalid local path: {key}");
        }
        Ok(self.root().join(rel))
    }

    fn list_sync(&self) -> Result<Vec<MangaEntry>> {
        let root = self.root();
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))?;
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() || is_archive(&path) {
                let title = if path.is_dir() { name.clone() } else { stem(&path) };
                out.push(MangaEntry { key: name.clone(), title, cover_url: format!("{COVER_PREFIX}{name}") });
            }
        }
        out.sort_by(|a, b| natural_cmp(&a.title, &b.title));
        Ok(out)
    }

    fn mtime(path: &Path) -> i64 {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn chapters_sync(&self, manga_key: &str) -> Result<Vec<ChapterEntry>> {
        let path = self.resolve(manga_key)?;
        if path.is_file() {
            return Ok(vec![ChapterEntry {
                key: manga_key.to_string(),
                title: stem(&path),
                number: Some(1.0),
                published: Self::mtime(&path),
                ..Default::default()
            }]);
        }
        let mut subs: Vec<(String, PathBuf)> = Vec::new();
        let mut has_images = false;
        for e in std::fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() || is_archive(&p) {
                subs.push((name, p));
            } else if is_image_name(&name) {
                has_images = true;
            }
        }
        if subs.is_empty() {
            if has_images {
                return Ok(vec![ChapterEntry {
                    key: manga_key.to_string(),
                    title: manga_title(&path),
                    number: Some(1.0),
                    published: Self::mtime(&path),
                    ..Default::default()
                }]);
            }
            return Ok(vec![]);
        }
        subs.sort_by(|a, b| natural_cmp(&b.0, &a.0)); // newest (highest) first
        Ok(subs
            .into_iter()
            .map(|(name, p)| {
                let title = if p.is_dir() { name.clone() } else { stem(&p) };
                ChapterEntry {
                    key: format!("{manga_key}/{name}"),
                    number: last_number(&title),
                    title,
                    volume: None,
                    scanlator: String::new(),
                    published: Self::mtime(&p),
                }
            })
            .collect())
    }

    fn pages_sync(&self, chapter_key: &str) -> Result<Vec<String>> {
        let path = self.resolve(chapter_key)?;
        if path.is_dir() {
            let mut files: Vec<String> = std::fs::read_dir(&path)?
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| is_image_name(n))
                .collect();
            files.sort_by(|a, b| natural_cmp(a, b));
            return Ok(files.into_iter().map(|f| format!("{FILE_PREFIX}{}", path.join(f).display())).collect());
        }
        let reader = ArchiveReader::open(&path)?;
        if reader.kind() == ArchiveKind::Zip {
            return reader.list_images();
        }
        // RAR/7z are solid: extract once into the cache and read files from there.
        let meta = std::fs::metadata(&path)?;
        let tag = stable_hash(&format!("{}:{}:{}", path.display(), meta.len(), Self::mtime(&path)));
        let dir = self.cache.join("archives").join(tag);
        let files = match cached_extraction(&dir) {
            Some(v) => v,
            None => {
                // Extract into a private temp dir, then move it into place, so a
                // concurrent reader never sees a half-written folder.
                let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&tmp);
                let v = reader.extract_all(&tmp)?;
                std::fs::write(tmp.join(DONE_MARKER), v.len().to_string())?;
                let _ = std::fs::remove_dir_all(&dir);
                if std::fs::rename(&tmp, &dir).is_err() {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
                cached_extraction(&dir).ok_or_else(|| anyhow!("could not unpack {}", path.display()))?
            }
        };
        Ok(files.into_iter().map(|p| format!("{FILE_PREFIX}{}", p.display())).collect())
    }

    fn image_sync(&self, chapter_key: &str, page: &str) -> Result<Vec<u8>> {
        if let Some(p) = page.strip_prefix(FILE_PREFIX) {
            let p = PathBuf::from(p);
            // `starts_with` is lexical, so `root/../etc` would pass without this.
            let no_parent = !p.components().any(|c| matches!(c, Component::ParentDir));
            let ok = no_parent && (p.starts_with(self.root()) || p.starts_with(&self.cache));
            if !ok {
                bail!("path outside library: {}", p.display());
            }
            return Ok(std::fs::read(&p)?);
        }
        ArchiveReader::open(&self.resolve(chapter_key)?)?.read(page)
    }

    fn cover_sync(&self, key: &str) -> Result<Vec<u8>> {
        let chapters = self.chapters_sync(key)?;
        // Oldest chapter's first page makes the best cover.
        let first = chapters.last().ok_or_else(|| anyhow!("no chapters"))?;
        let pages = self.pages_sync(&first.key)?;
        let page = pages.first().ok_or_else(|| anyhow!("no pages"))?;
        self.image_sync(&first.key, page)
    }
}

const DONE_MARKER: &str = ".done";

/// Files of a finished extraction, or `None` when it is missing or incomplete
/// (the cache trimmer deletes files individually, so a folder can lose pages).
fn cached_extraction(dir: &Path) -> Option<Vec<PathBuf>> {
    let expected: usize = std::fs::read_to_string(dir.join(DONE_MARKER)).ok()?.trim().parse().ok()?;
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| !n.to_string_lossy().starts_with('.')))
        .collect();
    if v.len() != expected || expected == 0 {
        return None;
    }
    v.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    Some(v)
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn manga_title(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// The last number appearing in a name, e.g. "Vol 2 Ch 12.5" -> 12.5.
pub fn last_number(name: &str) -> Option<f64> {
    let b: Vec<char> = name.chars().collect();
    let mut i = b.len();
    while i > 0 {
        if b[i - 1].is_ascii_digit() {
            let end = i;
            let mut start = i;
            while start > 0 && (b[start - 1].is_ascii_digit() || (b[start - 1] == '.' && start >= 2 && b[start - 2].is_ascii_digit())) {
                start -= 1;
            }
            let s: String = b[start..end].iter().collect();
            return s.trim_matches('.').parse().ok();
        }
        i -= 1;
    }
    None
}

fn paginate(all: Vec<MangaEntry>, page: u32) -> Paged<MangaEntry> {
    let start = (page.saturating_sub(1) as usize) * PAGE_SIZE;
    let has_next = all.len() > start + PAGE_SIZE;
    Paged { items: all.into_iter().skip(start).take(PAGE_SIZE).collect(), has_next }
}

#[async_trait]
impl Source for Local {
    fn id(&self) -> &'static str {
        "local"
    }
    fn name(&self) -> &'static str {
        "Local"
    }

    async fn search(&self, query: &str, page: u32) -> Result<Paged<MangaEntry>> {
        let q = query.trim().to_lowercase();
        let this = self.clone_paths();
        let all = tokio::task::spawn_blocking(move || this.list_sync()).await??;
        Ok(paginate(all.into_iter().filter(|m| m.title.to_lowercase().contains(&q)).collect(), page))
    }

    async fn latest(&self, page: u32) -> Result<Paged<MangaEntry>> {
        let this = self.clone_paths();
        let all = tokio::task::spawn_blocking(move || {
            let mut v = this.list_sync()?;
            v.sort_by_key(|m| std::cmp::Reverse(Self::mtime(&this.root().join(&m.key))));
            Ok::<_, anyhow::Error>(v)
        })
        .await??;
        Ok(paginate(all, page))
    }

    async fn popular(&self, page: u32) -> Result<Paged<MangaEntry>> {
        let this = self.clone_paths();
        let all = tokio::task::spawn_blocking(move || this.list_sync()).await??;
        Ok(paginate(all, page))
    }

    async fn details(&self, key: &str) -> Result<MangaDetails> {
        let path = self.resolve(key)?;
        if !path.exists() {
            bail!("{} no longer exists", path.display());
        }
        let title = if path.is_dir() { manga_title(&path) } else { stem(&path) };
        Ok(MangaDetails {
            title,
            cover_url: format!("{COVER_PREFIX}{key}"),
            description: format!("Local files at {}", path.display()),
            authors: String::new(),
            genres: vec![],
            status: Status::Unknown,
        })
    }

    async fn chapters(&self, manga_key: &str) -> Result<Vec<ChapterEntry>> {
        let (this, key) = (self.clone_paths(), manga_key.to_string());
        tokio::task::spawn_blocking(move || this.chapters_sync(&key)).await?
    }

    async fn pages(&self, chapter_key: &str) -> Result<Vec<String>> {
        let (this, key) = (self.clone_paths(), chapter_key.to_string());
        tokio::task::spawn_blocking(move || this.pages_sync(&key)).await?
    }

    async fn image(&self, chapter_key: &str, page: &str) -> Result<Vec<u8>> {
        let (this, key, page) = (self.clone_paths(), chapter_key.to_string(), page.to_string());
        tokio::task::spawn_blocking(move || this.image_sync(&key, &page)).await?
    }

    async fn cover(&self, url: &str) -> Result<Vec<u8>> {
        let key = url.strip_prefix(COVER_PREFIX).ok_or_else(|| anyhow!("not a local cover url"))?.to_string();
        let this = self.clone_paths();
        tokio::task::spawn_blocking(move || this.cover_sync(&key)).await?
    }

    fn set_option(&self, key: &str, value: &str) {
        if key == "local_dir" && !value.is_empty() {
            if let Ok(mut r) = self.root.write() {
                *r = PathBuf::from(value);
            }
        }
    }
}

impl Local {
    fn clone_paths(&self) -> Local {
        Local { root: RwLock::new(self.root()), cache: self.cache.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zip(path: &Path, names: &[&str]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for n in names {
            w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(n.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Local) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        // Folder series with folder chapters
        for ch in ["Chapter 1", "Chapter 2", "Chapter 10"] {
            let d = root.join("Alpha").join(ch);
            std::fs::create_dir_all(&d).unwrap();
            for p in ["2.jpg", "10.jpg", "1.jpg"] {
                std::fs::write(d.join(p), format!("{ch}/{p}")).unwrap();
            }
        }
        // Series of archives
        std::fs::create_dir_all(root.join("Beta")).unwrap();
        write_zip(&root.join("Beta/Vol 1.cbz"), &["b.png", "a.png"]);
        write_zip(&root.join("Beta/Vol 2.cbz"), &["a.png"]);
        // Single archive
        write_zip(&root.join("Gamma.cbz"), &["01.jpg", "02.jpg"]);
        // Folder of images
        std::fs::create_dir_all(root.join("Delta")).unwrap();
        std::fs::write(root.join("Delta/1.png"), b"d1").unwrap();
        std::fs::write(root.join(".hidden"), b"").unwrap();
        let src = Local::new(root, dir.path().join("cache"));
        (dir, src)
    }

    #[tokio::test]
    async fn lists_and_searches() {
        let (_d, src) = fixture();
        let all = src.popular(1).await.unwrap();
        let titles: Vec<_> = all.items.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["Alpha", "Beta", "Delta", "Gamma"]);
        assert!(!all.has_next);
        let found = src.search("gam", 1).await.unwrap();
        assert_eq!(found.items.len(), 1);
        assert_eq!(found.items[0].key, "Gamma.cbz");
        assert_eq!(src.latest(1).await.unwrap().items.len(), 4);
    }

    #[tokio::test]
    async fn folder_chapters_are_naturally_ordered_newest_first() {
        let (_d, src) = fixture();
        let ch = src.chapters("Alpha").await.unwrap();
        let names: Vec<_> = ch.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(names, vec!["Chapter 10", "Chapter 2", "Chapter 1"]);
        assert_eq!(ch[0].number, Some(10.0));
        let pages = src.pages(&ch[0].key).await.unwrap();
        assert_eq!(pages.len(), 3);
        assert!(pages[0].ends_with("1.jpg") && pages[2].ends_with("10.jpg"));
        assert_eq!(src.image(&ch[0].key, &pages[1]).await.unwrap(), b"Chapter 10/2.jpg");
    }

    #[tokio::test]
    async fn archive_chapters_and_single_archive() {
        let (_d, src) = fixture();
        let ch = src.chapters("Beta").await.unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].title, "Vol 2");
        let pages = src.pages(&ch[1].key).await.unwrap();
        assert_eq!(pages, vec!["a.png", "b.png"]);
        assert_eq!(src.image(&ch[1].key, "b.png").await.unwrap(), b"b.png");

        let single = src.chapters("Gamma.cbz").await.unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(src.pages(&single[0].key).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn image_folder_is_single_chapter_and_cover_works() {
        let (_d, src) = fixture();
        let ch = src.chapters("Delta").await.unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].key, "Delta");
        assert_eq!(src.cover("local-cover:Delta").await.unwrap(), b"d1");
        assert_eq!(src.cover("local-cover:Beta").await.unwrap(), b"a.png"); // oldest chapter, first page
        assert!(src.cover("https://x").await.is_err());
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (_d, src) = fixture();
        assert!(src.chapters("../lib").await.is_err());
        assert!(src.pages("/etc").await.is_err());
        assert!(src.image("Alpha", "file:/etc/passwd").await.is_err());
        let sneaky = format!("file:{}/../../etc/passwd", src.root().display());
        assert!(src.image("Alpha", &sneaky).await.is_err());
    }

    #[tokio::test]
    async fn trimmed_extraction_cache_is_rebuilt() {
        let (d, src) = fixture();
        let p = src.root().join("Seven.cb7");
        let tmp = d.path().join("seven-src");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("1.png"), b"one").unwrap();
        std::fs::write(tmp.join("2.png"), b"two").unwrap();
        sevenz_rust2::compress_to_path(&tmp, &p).unwrap();
        let pages = src.pages("Seven.cb7").await.unwrap();
        assert_eq!(pages.len(), 2);
        // the cache trimmer removed one extracted page
        std::fs::remove_file(pages[1].strip_prefix(FILE_PREFIX).unwrap()).unwrap();
        let again = src.pages("Seven.cb7").await.unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(src.image("Seven.cb7", &again[1]).await.unwrap(), b"two");
    }

    #[test]
    fn number_extraction() {
        assert_eq!(last_number("Chapter 12.5"), Some(12.5));
        assert_eq!(last_number("Vol 2 Ch 12"), Some(12.0));
        assert_eq!(last_number("Prologue"), None);
        assert_eq!(last_number("v01"), Some(1.0));
    }
}
