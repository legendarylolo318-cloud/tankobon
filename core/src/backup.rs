//! Library backup/restore into a single file (a ZIP holding `backup.json`).

use crate::db::{Db, LibraryQuery};
use crate::models::*;
use crate::settings::Settings;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

const VERSION: u32 = 1;
const ENTRY: &str = "backup.json";

#[derive(Serialize, Deserialize)]
struct Backup {
    version: u32,
    created: i64,
    settings: Settings,
    categories: Vec<String>,
    manga: Vec<BackupManga>,
}

#[derive(Serialize, Deserialize)]
struct BackupManga {
    source: String,
    key: String,
    title: String,
    cover_url: String,
    description: String,
    authors: String,
    genres: Vec<String>,
    status: Status,
    added_at: i64,
    categories: Vec<String>,
    reader_override: Option<ReaderSettings>,
    chapters: Vec<BackupChapter>,
}

#[derive(Serialize, Deserialize)]
struct BackupChapter {
    key: String,
    title: String,
    number: Option<f64>,
    volume: Option<String>,
    scanlator: String,
    published: i64,
    read: bool,
    last_page: i32,
    page_count: i32,
    last_read_at: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RestoreStats {
    pub manga: usize,
    pub chapters: usize,
    pub categories: usize,
}

pub fn export(db: &Db, path: &Path) -> Result<()> {
    let mut manga = Vec::new();
    for e in db.library(&LibraryQuery::default())? {
        let m = e.manga;
        let chapters = db
            .chapters(m.id)?
            .into_iter()
            .map(|c| BackupChapter {
                key: c.key,
                title: c.title,
                number: c.number,
                volume: c.volume,
                scanlator: c.scanlator,
                published: c.published,
                read: c.read,
                last_page: c.last_page,
                page_count: c.page_count,
                last_read_at: c.last_read_at,
            })
            .collect();
        manga.push(BackupManga {
            categories: db.category_names_for(m.id)?,
            source: m.source,
            key: m.key,
            title: m.title,
            cover_url: m.cover_url,
            description: m.description,
            authors: m.authors,
            genres: m.genres,
            status: m.status,
            added_at: m.added_at,
            reader_override: m.reader_override,
            chapters,
        });
    }
    let backup = Backup {
        version: VERSION,
        created: crate::util::now(),
        settings: db.load_settings(),
        categories: db.categories()?.into_iter().map(|c| c.name).collect(),
        manga,
    };
    let tmp = path.with_extension("tmp");
    {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
        zip.start_file(ENTRY, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated))?;
        zip.write_all(&serde_json::to_vec(&backup)?)?;
        zip.finish()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Restore a backup, merging into the current library. Machine-specific
/// directories in the settings are kept as they are.
pub fn restore(db: &Db, path: &Path) -> Result<RestoreStats> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?)
        .context("not a Tankobon backup file")?;
    let mut json = String::new();
    zip.by_name(ENTRY).context("backup.json missing; not a Tankobon backup")?.read_to_string(&mut json)?;
    let backup: Backup = serde_json::from_str(&json).context("corrupt backup")?;
    if backup.version > VERSION {
        bail!("backup was made by a newer version (format {})", backup.version);
    }
    let mut stats = RestoreStats::default();
    for name in &backup.categories {
        db.ensure_category(name)?;
        stats.categories += 1;
    }
    for bm in backup.manga {
        let id = db.upsert_manga(&Manga {
            source: bm.source,
            key: bm.key,
            title: bm.title,
            cover_url: bm.cover_url,
            description: bm.description,
            authors: bm.authors,
            genres: bm.genres,
            status: bm.status,
            ..Default::default()
        })?;
        db.set_in_library(&[id], true)?;
        let cat_ids = bm.categories.iter().map(|n| db.ensure_category(n)).collect::<Result<Vec<_>>>()?;
        db.set_manga_categories(id, &cat_ids)?;
        db.set_reader_override(id, bm.reader_override.as_ref())?;
        let fetched: Vec<Chapter> = bm
            .chapters
            .iter()
            .enumerate()
            .map(|(i, c)| Chapter {
                key: c.key.clone(),
                title: c.title.clone(),
                number: c.number,
                volume: c.volume.clone(),
                scanlator: c.scanlator.clone(),
                published: c.published,
                position: i as i32,
                ..Default::default()
            })
            .collect();
        db.merge_chapters(id, &fetched)?;
        db.clear_new(id)?;
        let existing = db.chapters(id)?;
        for c in &bm.chapters {
            if let Some(row) = existing.iter().find(|e| e.key == c.key) {
                db.update_chapter_state(row.id, c.read, c.last_page, c.last_read_at)?;
                if c.page_count > 0 {
                    // keep page counts so progress bars survive the restore
                    db.set_downloaded(row.id, row.downloaded, Some(c.page_count))?;
                }
                stats.chapters += 1;
            }
        }
        stats.manga += 1;
    }
    let mut s = backup.settings;
    let current = db.load_settings();
    s.download_dir = current.download_dir;
    s.local_dir = current.local_dir;
    db.save_settings(&s)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Db {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_manga(&Manga { source: "s".into(), key: "k".into(), title: "T".into(), genres: vec!["Action".into()], ..Default::default() }).unwrap();
        db.set_in_library(&[id], true).unwrap();
        let chapters: Vec<Chapter> = (1..=3).rev().map(|i| Chapter { key: format!("c{i}"), number: Some(i as f64), ..Default::default() }).collect();
        db.merge_chapters(id, &chapters).unwrap();
        let cat = db.add_category("Fav").unwrap();
        db.move_to_category(&[id], Some(cat)).unwrap();
        let chs = db.chapters(id).unwrap();
        db.set_progress(chs[2].id, 9, 10).unwrap(); // read
        db.set_progress(chs[1].id, 4, 10).unwrap(); // in progress
        db.set_reader_override(id, Some(&ReaderSettings { mode: ReaderMode::Webtoon, ..Default::default() })).unwrap();
        db.save_settings(&Settings { cache_limit_mb: 77, download_dir: "/machine/a".into(), ..Default::default() }).unwrap();
        db
    }

    #[test]
    fn export_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.tankobon");
        export(&seeded(), &file).unwrap();

        let fresh = Db::open_memory().unwrap();
        fresh.save_settings(&Settings { download_dir: "/machine/b".into(), ..Default::default() }).unwrap();
        let stats = restore(&fresh, &file).unwrap();
        assert_eq!(stats, RestoreStats { manga: 1, chapters: 3, categories: 1 });

        let lib = fresh.library(&LibraryQuery::default()).unwrap();
        assert_eq!(lib.len(), 1);
        let e = &lib[0];
        assert_eq!(e.manga.title, "T");
        assert_eq!(e.manga.genres, vec!["Action"]);
        assert_eq!(e.unread, 2);
        assert_eq!(e.new_count, 0);
        assert_eq!(e.category_ids.len(), 1);
        assert_eq!(e.manga.reader_override.as_ref().unwrap().mode, ReaderMode::Webtoon);
        let chs = fresh.chapters(e.manga.id).unwrap();
        assert!(chs[2].read);
        assert_eq!(chs[1].last_page, 4);
        assert!(!chs[1].read);
        let s = fresh.load_settings();
        assert_eq!(s.cache_limit_mb, 77);
        assert_eq!(s.download_dir, "/machine/b", "machine-specific paths are kept");
    }

    #[test]
    fn restore_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("b.tankobon");
        let db = seeded();
        export(&db, &file).unwrap();
        restore(&db, &file).unwrap();
        restore(&db, &file).unwrap();
        assert_eq!(db.library(&LibraryQuery::default()).unwrap().len(), 1);
        assert_eq!(db.categories().unwrap().len(), 1);
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x");
        std::fs::write(&file, b"nope").unwrap();
        assert!(restore(&Db::open_memory().unwrap(), &file).is_err());
        assert!(restore(&Db::open_memory().unwrap(), &dir.path().join("missing")).is_err());
    }
}
