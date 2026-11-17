//! SQLite persistence layer.

use crate::models::*;
use crate::settings::Settings;
use crate::util::now;
use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS manga (
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL,
    key TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    cover_url TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    authors TEXT NOT NULL DEFAULT '',
    genres TEXT NOT NULL DEFAULT '[]',
    status INTEGER NOT NULL DEFAULT 0,
    in_library INTEGER NOT NULL DEFAULT 0,
    added_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0,
    reader_override TEXT,
    UNIQUE(source, key)
);
CREATE TABLE IF NOT EXISTS chapter (
    id INTEGER PRIMARY KEY,
    manga_id INTEGER NOT NULL REFERENCES manga(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    number REAL,
    volume TEXT,
    scanlator TEXT NOT NULL DEFAULT '',
    published INTEGER NOT NULL DEFAULT 0,
    read INTEGER NOT NULL DEFAULT 0,
    last_page INTEGER NOT NULL DEFAULT 0,
    page_count INTEGER NOT NULL DEFAULT 0,
    downloaded INTEGER NOT NULL DEFAULT 0,
    is_new INTEGER NOT NULL DEFAULT 0,
    found_at INTEGER NOT NULL DEFAULT 0,
    last_read_at INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL DEFAULT 0,
    UNIQUE(manga_id, key)
);
CREATE INDEX IF NOT EXISTS idx_chapter_manga ON chapter(manga_id);
CREATE INDEX IF NOT EXISTS idx_chapter_history ON chapter(last_read_at);
CREATE TABLE IF NOT EXISTS category (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    position INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS manga_category (
    manga_id INTEGER NOT NULL REFERENCES manga(id) ON DELETE CASCADE,
    category_id INTEGER NOT NULL REFERENCES category(id) ON DELETE CASCADE,
    PRIMARY KEY (manga_id, category_id)
);
CREATE TABLE IF NOT EXISTS download (
    chapter_id INTEGER PRIMARY KEY REFERENCES chapter(id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    done_pages INTEGER NOT NULL DEFAULT 0,
    total_pages INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    queued_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

const MANGA_COLS: &str = "m.id, m.source, m.key, m.title, m.cover_url, m.description, m.authors, m.genres, m.status, m.in_library, m.added_at, m.updated_at, m.reader_override";
const CHAPTER_COLS: &str = "c.id, c.manga_id, c.key, c.title, c.number, c.volume, c.scanlator, c.published, c.read, c.last_page, c.page_count, c.downloaded, c.is_new, c.found_at, c.last_read_at, c.position";

fn manga_from_row(r: &Row) -> rusqlite::Result<Manga> {
    let genres: String = r.get(7)?;
    let ov: Option<String> = r.get(12)?;
    Ok(Manga {
        id: r.get(0)?,
        source: r.get(1)?,
        key: r.get(2)?,
        title: r.get(3)?,
        cover_url: r.get(4)?,
        description: r.get(5)?,
        authors: r.get(6)?,
        genres: serde_json::from_str(&genres).unwrap_or_default(),
        status: Status::from_i32(r.get(8)?),
        in_library: r.get::<_, i64>(9)? != 0,
        added_at: r.get(10)?,
        updated_at: r.get(11)?,
        reader_override: ov.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

fn chapter_from_row(r: &Row) -> rusqlite::Result<Chapter> {
    Ok(Chapter {
        id: r.get(0)?,
        manga_id: r.get(1)?,
        key: r.get(2)?,
        title: r.get(3)?,
        number: r.get(4)?,
        volume: r.get(5)?,
        scanlator: r.get(6)?,
        published: r.get(7)?,
        read: r.get::<_, i64>(8)? != 0,
        last_page: r.get(9)?,
        page_count: r.get(10)?,
        downloaded: r.get::<_, i64>(11)? != 0,
        is_new: r.get::<_, i64>(12)? != 0,
        found_at: r.get(13)?,
        last_read_at: r.get(14)?,
        position: r.get(15)?,
    })
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Query parameters for the library view.
#[derive(Debug, Clone, Default)]
pub struct LibraryQuery {
    pub search: String,
    /// `None` = all; `Some(0)` = uncategorized; otherwise a category id.
    pub category: Option<i64>,
    pub sort: LibrarySort,
    pub descending: bool,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- settings -------------------------------------------------------

    pub fn load_settings(&self) -> Settings {
        let c = self.c();
        c.query_row("SELECT value FROM kv WHERE key='settings'", [], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        let json = serde_json::to_string(s)?;
        self.c().execute(
            "INSERT INTO kv(key,value) VALUES('settings',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [json],
        )?;
        Ok(())
    }

    // ---- manga ----------------------------------------------------------

    /// Insert or refresh a manga's metadata; never changes library membership.
    pub fn upsert_manga(&self, m: &Manga) -> Result<i64> {
        let c = self.c();
        let genres = serde_json::to_string(&m.genres)?;
        c.execute(
            "INSERT INTO manga(source,key,title,cover_url,description,authors,genres,status,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(source,key) DO UPDATE SET
               title=CASE WHEN excluded.title<>'' THEN excluded.title ELSE title END,
               cover_url=CASE WHEN excluded.cover_url<>'' THEN excluded.cover_url ELSE cover_url END,
               description=CASE WHEN excluded.description<>'' THEN excluded.description ELSE description END,
               authors=CASE WHEN excluded.authors<>'' THEN excluded.authors ELSE authors END,
               genres=CASE WHEN excluded.genres<>'[]' THEN excluded.genres ELSE genres END,
               status=CASE WHEN excluded.status<>0 THEN excluded.status ELSE status END",
            params![m.source, m.key, m.title, m.cover_url, m.description, m.authors, genres, m.status.as_i32(), now()],
        )?;
        Ok(c.query_row("SELECT id FROM manga WHERE source=?1 AND key=?2", params![m.source, m.key], |r| r.get(0))?)
    }

    pub fn get_manga(&self, id: i64) -> Result<Option<Manga>> {
        let c = self.c();
        Ok(c.query_row(&format!("SELECT {MANGA_COLS} FROM manga m WHERE m.id=?1"), [id], manga_from_row).optional()?)
    }

    pub fn find_manga(&self, source: &str, key: &str) -> Result<Option<Manga>> {
        let c = self.c();
        Ok(c.query_row(
            &format!("SELECT {MANGA_COLS} FROM manga m WHERE m.source=?1 AND m.key=?2"),
            params![source, key],
            manga_from_row,
        )
        .optional()?)
    }

    pub fn set_in_library(&self, ids: &[i64], in_library: bool) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for id in ids {
            if in_library {
                tx.execute(
                    "UPDATE manga SET in_library=1, added_at=CASE WHEN added_at=0 THEN ?2 ELSE added_at END WHERE id=?1",
                    params![id, now()],
                )?;
            } else {
                tx.execute("UPDATE manga SET in_library=0 WHERE id=?1", [id])?;
                tx.execute("DELETE FROM manga_category WHERE manga_id=?1", [id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove manga rows that are neither in the library nor referenced by history/downloads.
    pub fn purge_orphans(&self) -> Result<usize> {
        Ok(self.c().execute(
            "DELETE FROM manga WHERE in_library=0
               AND NOT EXISTS (SELECT 1 FROM chapter c WHERE c.manga_id=manga.id AND (c.last_read_at>0 OR c.downloaded=1))
               AND NOT EXISTS (SELECT 1 FROM chapter c JOIN download d ON d.chapter_id=c.id WHERE c.manga_id=manga.id)",
            [],
        )?)
    }

    pub fn set_reader_override(&self, manga_id: i64, s: Option<&ReaderSettings>) -> Result<()> {
        let json = s.map(serde_json::to_string).transpose()?;
        self.c().execute("UPDATE manga SET reader_override=?2 WHERE id=?1", params![manga_id, json])?;
        Ok(())
    }

    pub fn library(&self, q: &LibraryQuery) -> Result<Vec<LibraryEntry>> {
        let c = self.c();
        let mut sql = format!(
            "SELECT {MANGA_COLS},
               (SELECT COUNT(*) FROM chapter c WHERE c.manga_id=m.id AND c.read=0),
               (SELECT COUNT(*) FROM chapter c WHERE c.manga_id=m.id),
               (SELECT COUNT(*) FROM chapter c WHERE c.manga_id=m.id AND c.is_new=1),
               (SELECT COUNT(*) FROM chapter c WHERE c.manga_id=m.id AND c.downloaded=1),
               COALESCE((SELECT MAX(last_read_at) FROM chapter c WHERE c.manga_id=m.id),0),
               COALESCE((SELECT MAX(found_at) FROM chapter c WHERE c.manga_id=m.id),0) AS lu
             FROM manga m WHERE m.in_library=1"
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !q.search.trim().is_empty() {
            sql.push_str(" AND (m.title LIKE ? ESCAPE '\\' OR m.authors LIKE ? ESCAPE '\\')");
            let pat = format!("%{}%", q.search.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            args.push(pat.clone().into());
            args.push(pat.into());
        }
        match q.category {
            Some(0) => sql.push_str(" AND NOT EXISTS (SELECT 1 FROM manga_category mc WHERE mc.manga_id=m.id)"),
            Some(cat) => {
                sql.push_str(" AND EXISTS (SELECT 1 FROM manga_category mc WHERE mc.manga_id=m.id AND mc.category_id=?)");
                args.push(cat.into());
            }
            None => {}
        }
        let dir = if q.descending { "DESC" } else { "ASC" };
        let order = match q.sort {
            LibrarySort::Title => format!("m.title COLLATE NOCASE {dir}"),
            LibrarySort::LastRead => format!("COALESCE((SELECT MAX(last_read_at) FROM chapter c WHERE c.manga_id=m.id),0) {dir}, m.title COLLATE NOCASE"),
            LibrarySort::LastUpdated => format!("lu {dir}, m.title COLLATE NOCASE"),
            LibrarySort::DateAdded => format!("m.added_at {dir}, m.title COLLATE NOCASE"),
            LibrarySort::Unread => format!("(SELECT COUNT(*) FROM chapter c WHERE c.manga_id=m.id AND c.read=0) {dir}, m.title COLLATE NOCASE"),
        };
        sql.push_str(&format!(" ORDER BY {order}"));
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
            Ok(LibraryEntry {
                manga: manga_from_row(r)?,
                unread: r.get(13)?,
                total: r.get(14)?,
                new_count: r.get(15)?,
                downloaded: r.get(16)?,
                last_read_at: r.get(17)?,
                category_ids: vec![],
            })
        })?;
        let mut out: Vec<LibraryEntry> = rows.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut cs = c.prepare("SELECT category_id FROM manga_category WHERE manga_id=?1")?;
        for e in &mut out {
            e.category_ids = cs.query_map([e.manga.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        }
        Ok(out)
    }

    pub fn library_ids(&self) -> Result<Vec<i64>> {
        let c = self.c();
        let mut s = c.prepare("SELECT id FROM manga WHERE in_library=1 ORDER BY title COLLATE NOCASE")?;
        let v = s.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- categories -----------------------------------------------------

    pub fn categories(&self) -> Result<Vec<Category>> {
        let c = self.c();
        let mut s = c.prepare("SELECT id,name,position FROM category ORDER BY position, name COLLATE NOCASE")?;
        let v = s
            .query_map([], |r| Ok(Category { id: r.get(0)?, name: r.get(1)?, position: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn add_category(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "category name is empty");
        let c = self.c();
        let pos: i64 = c.query_row("SELECT COALESCE(MAX(position)+1,0) FROM category", [], |r| r.get(0))?;
        c.execute("INSERT INTO category(name,position) VALUES(?1,?2)", params![name, pos])?;
        Ok(c.last_insert_rowid())
    }

    pub fn rename_category(&self, id: i64, name: &str) -> Result<()> {
        self.c().execute("UPDATE category SET name=?2 WHERE id=?1", params![id, name.trim()])?;
        Ok(())
    }

    pub fn delete_category(&self, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM category WHERE id=?1", [id])?;
        Ok(())
    }

    /// Replace the manga's category membership with exactly `category` (or none when `None`).
    pub fn move_to_category(&self, manga_ids: &[i64], category: Option<i64>) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for id in manga_ids {
            tx.execute("DELETE FROM manga_category WHERE manga_id=?1", [id])?;
            if let Some(cat) = category {
                tx.execute("INSERT OR IGNORE INTO manga_category(manga_id,category_id) VALUES(?1,?2)", params![id, cat])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- chapters -------------------------------------------------------

    pub fn chapters(&self, manga_id: i64) -> Result<Vec<Chapter>> {
        let c = self.c();
        let mut s = c.prepare(&format!("SELECT {CHAPTER_COLS} FROM chapter c WHERE c.manga_id=?1 ORDER BY c.position"))?;
        let v = s.query_map([manga_id], chapter_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn get_chapter(&self, id: i64) -> Result<Option<Chapter>> {
        let c = self.c();
        Ok(c.query_row(&format!("SELECT {CHAPTER_COLS} FROM chapter c WHERE c.id=?1"), [id], chapter_from_row).optional()?)
    }

    /// Merge a freshly fetched chapter list (newest first) into the database.
    /// Returns the ids of chapters that did not exist before. When the manga had no
    /// chapters beforehand, nothing is flagged "new".
    pub fn merge_chapters(&self, manga_id: i64, fetched: &[Chapter]) -> Result<Vec<i64>> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let had: i64 = tx.query_row("SELECT COUNT(*) FROM chapter WHERE manga_id=?1", [manga_id], |r| r.get(0))?;
        let ts = now();
        let mut new_ids = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::with_capacity(fetched.len());
        for (pos, ch) in fetched.iter().enumerate() {
            seen.insert(ch.key.as_str());
            let existing: Option<i64> = tx
                .query_row("SELECT id FROM chapter WHERE manga_id=?1 AND key=?2", params![manga_id, ch.key], |r| r.get(0))
                .optional()?;
            match existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE chapter SET title=?2, number=?3, volume=?4, scanlator=?5, published=?6, position=?7 WHERE id=?1",
                        params![id, ch.title, ch.number, ch.volume, ch.scanlator, ch.published, pos as i64],
                    )?;
                }
                None => {
                    let flag = had > 0;
                    tx.execute(
                        "INSERT INTO chapter(manga_id,key,title,number,volume,scanlator,published,is_new,found_at,position)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                        params![manga_id, ch.key, ch.title, ch.number, ch.volume, ch.scanlator, ch.published, flag as i64, ts, pos as i64],
                    )?;
                    if flag {
                        new_ids.push(tx.last_insert_rowid());
                    }
                }
            }
        }
        // Drop chapters that vanished from the source unless they carry local state.
        if !fetched.is_empty() {
            let mut stmt = tx.prepare("SELECT id,key FROM chapter WHERE manga_id=?1 AND downloaded=0 AND last_read_at=0 AND read=0")?;
            let stale: Vec<i64> = stmt
                .query_map([manga_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .filter_map(|r| r.ok())
                .filter(|(_, k)| !seen.contains(k.as_str()))
                .map(|(i, _)| i)
                .collect();
            drop(stmt);
            for id in stale {
                tx.execute("DELETE FROM chapter WHERE id=?1", [id])?;
            }
        }
        tx.execute("UPDATE manga SET updated_at=?2 WHERE id=?1", params![manga_id, ts])?;
        tx.commit()?;
        Ok(new_ids)
    }

    pub fn set_read(&self, chapter_ids: &[i64], read: bool) -> Result<()> {
        if chapter_ids.is_empty() {
            return Ok(());
        }
        let sql = if read {
            format!("UPDATE chapter SET read=1, is_new=0 WHERE id IN ({})", placeholders(chapter_ids.len()))
        } else {
            format!("UPDATE chapter SET read=0, last_page=0 WHERE id IN ({})", placeholders(chapter_ids.len()))
        };
        self.c().execute(&sql, params_from_iter(chapter_ids.iter()))?;
        Ok(())
    }

    /// Mark every chapter of the given manga read/unread.
    pub fn set_manga_read(&self, manga_ids: &[i64], read: bool) -> Result<()> {
        if manga_ids.is_empty() {
            return Ok(());
        }
        let ph = placeholders(manga_ids.len());
        let sql = if read {
            format!("UPDATE chapter SET read=1, is_new=0 WHERE manga_id IN ({ph})")
        } else {
            format!("UPDATE chapter SET read=0, last_page=0 WHERE manga_id IN ({ph})")
        };
        self.c().execute(&sql, params_from_iter(manga_ids.iter()))?;
        Ok(())
    }

    pub fn clear_all_new(&self) -> Result<()> {
        self.c().execute("UPDATE chapter SET is_new=0 WHERE is_new=1", [])?;
        Ok(())
    }

    pub fn clear_new(&self, manga_id: i64) -> Result<()> {
        self.c().execute("UPDATE chapter SET is_new=0 WHERE manga_id=?1", [manga_id])?;
        Ok(())
    }

    /// Persist reading progress; the chapter becomes read when the last page is reached.
    pub fn set_progress(&self, chapter_id: i64, page: i32, page_count: i32) -> Result<()> {
        let finished = page_count > 0 && page >= page_count - 1;
        self.c().execute(
            "UPDATE chapter SET last_page=?2, page_count=?3, last_read_at=?4,
               read=CASE WHEN ?5=1 THEN 1 ELSE read END,
               is_new=CASE WHEN ?5=1 THEN 0 ELSE is_new END WHERE id=?1",
            params![chapter_id, page, page_count, now(), finished as i64],
        )?;
        Ok(())
    }

    pub fn set_downloaded(&self, chapter_id: i64, downloaded: bool, page_count: Option<i32>) -> Result<()> {
        self.c().execute(
            "UPDATE chapter SET downloaded=?2, page_count=COALESCE(?3,page_count) WHERE id=?1",
            params![chapter_id, downloaded as i64, page_count],
        )?;
        Ok(())
    }

    pub fn downloaded_chapter_ids(&self, manga_ids: &[i64]) -> Result<Vec<i64>> {
        if manga_ids.is_empty() {
            return Ok(vec![]);
        }
        let c = self.c();
        let mut s = c.prepare(&format!(
            "SELECT id FROM chapter WHERE downloaded=1 AND manga_id IN ({})",
            placeholders(manga_ids.len())
        ))?;
        let v = s.query_map(params_from_iter(manga_ids.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// Unread, not downloaded chapter ids for the given manga, oldest first.
    pub fn undownloaded_unread(&self, manga_ids: &[i64]) -> Result<Vec<i64>> {
        if manga_ids.is_empty() {
            return Ok(vec![]);
        }
        let c = self.c();
        let mut s = c.prepare(&format!(
            "SELECT id FROM chapter WHERE downloaded=0 AND read=0 AND manga_id IN ({}) ORDER BY manga_id, position DESC",
            placeholders(manga_ids.len())
        ))?;
        let v = s.query_map(params_from_iter(manga_ids.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- history & updates ---------------------------------------------

    /// Most recently read chapter for each manga, newest first.
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let c = self.c();
        let sql = format!(
            "SELECT {CHAPTER_COLS}, {MANGA_COLS} FROM chapter c JOIN manga m ON m.id=c.manga_id
             WHERE c.last_read_at>0 AND c.last_read_at=(SELECT MAX(c2.last_read_at) FROM chapter c2 WHERE c2.manga_id=c.manga_id)
             GROUP BY c.manga_id ORDER BY c.last_read_at DESC LIMIT ?1"
        );
        let mut s = c.prepare(&sql)?;
        let v = s
            .query_map([limit as i64], |r| {
                let chapter = chapter_from_row(r)?;
                // manga columns start at offset 16
                let genres: String = r.get(16 + 7)?;
                let ov: Option<String> = r.get(16 + 12)?;
                let manga = Manga {
                    id: r.get(16)?,
                    source: r.get(17)?,
                    key: r.get(18)?,
                    title: r.get(19)?,
                    cover_url: r.get(20)?,
                    description: r.get(21)?,
                    authors: r.get(22)?,
                    genres: serde_json::from_str(&genres).unwrap_or_default(),
                    status: Status::from_i32(r.get(24)?),
                    in_library: r.get::<_, i64>(25)? != 0,
                    added_at: r.get(26)?,
                    updated_at: r.get(27)?,
                    reader_override: ov.and_then(|s| serde_json::from_str(&s).ok()),
                };
                Ok(HistoryEntry { chapter, manga })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.c().execute("UPDATE chapter SET last_read_at=0 WHERE last_read_at>0", [])?;
        Ok(())
    }

    pub fn remove_history_for_manga(&self, manga_id: i64) -> Result<()> {
        self.c().execute("UPDATE chapter SET last_read_at=0 WHERE manga_id=?1", [manga_id])?;
        Ok(())
    }

    /// Chapters discovered by update checks, newest first (library manga only).
    pub fn recent_new_chapters(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let c = self.c();
        let sql = format!(
            "SELECT {CHAPTER_COLS}, {MANGA_COLS} FROM chapter c JOIN manga m ON m.id=c.manga_id
             WHERE c.is_new=1 AND m.in_library=1 ORDER BY c.found_at DESC, c.position ASC LIMIT ?1"
        );
        let mut s = c.prepare(&sql)?;
        let v = s
            .query_map([limit as i64], |r| {
                let chapter = chapter_from_row(r)?;
                let genres: String = r.get(16 + 7)?;
                let manga = Manga {
                    id: r.get(16)?,
                    source: r.get(17)?,
                    key: r.get(18)?,
                    title: r.get(19)?,
                    cover_url: r.get(20)?,
                    genres: serde_json::from_str(&genres).unwrap_or_default(),
                    in_library: true,
                    ..Default::default()
                };
                Ok(HistoryEntry { chapter, manga })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    // ---- downloads ------------------------------------------------------

    pub fn enqueue_download(&self, chapter_id: i64) -> Result<bool> {
        let c = self.c();
        let downloaded: i64 = c.query_row("SELECT downloaded FROM chapter WHERE id=?1", [chapter_id], |r| r.get(0))?;
        if downloaded != 0 {
            return Ok(false);
        }
        let n = c.execute(
            "INSERT INTO download(chapter_id,state,queued_at) VALUES(?1,'queued',?2)
             ON CONFLICT(chapter_id) DO UPDATE SET state='queued', error='' WHERE state IN ('failed','paused')",
            params![chapter_id, now()],
        )?;
        Ok(n > 0)
    }

    pub fn set_download_state(&self, chapter_id: i64, state: DownloadState, error: &str) -> Result<()> {
        self.c().execute("UPDATE download SET state=?2, error=?3 WHERE chapter_id=?1", params![chapter_id, state.as_str(), error])?;
        Ok(())
    }

    /// Atomically move a job from `queued` to `downloading`. Returns false when the
    /// job was paused, cancelled or claimed by someone else in the meantime.
    pub fn claim_download(&self, chapter_id: i64) -> Result<bool> {
        let n = self.c().execute("UPDATE download SET state='downloading', error='' WHERE chapter_id=?1 AND state='queued'", [chapter_id])?;
        Ok(n > 0)
    }

    pub fn set_download_progress(&self, chapter_id: i64, done: i32, total: i32) -> Result<()> {
        self.c().execute("UPDATE download SET done_pages=?2, total_pages=?3 WHERE chapter_id=?1", params![chapter_id, done, total])?;
        Ok(())
    }

    pub fn remove_download(&self, chapter_id: i64) -> Result<()> {
        self.c().execute("DELETE FROM download WHERE chapter_id=?1", [chapter_id])?;
        Ok(())
    }

    /// Jobs in queue order; finished jobs are not listed.
    pub fn download_jobs(&self) -> Result<Vec<DownloadJob>> {
        let c = self.c();
        let mut s = c.prepare(
            "SELECT d.chapter_id, c.manga_id, d.state, d.done_pages, d.total_pages, d.error, m.title, c.title, c.number, c.volume
             FROM download d JOIN chapter c ON c.id=d.chapter_id JOIN manga m ON m.id=c.manga_id
             WHERE d.state<>'done' ORDER BY d.queued_at, d.chapter_id",
        )?;
        let v = s
            .query_map([], |r| {
                let ch = Chapter {
                    title: r.get(7)?,
                    number: r.get(8)?,
                    volume: r.get(9)?,
                    ..Default::default()
                };
                Ok(DownloadJob {
                    chapter_id: r.get(0)?,
                    manga_id: r.get(1)?,
                    state: DownloadState::parse(&r.get::<_, String>(2)?),
                    done_pages: r.get(3)?,
                    total_pages: r.get(4)?,
                    error: r.get(5)?,
                    manga_title: r.get(6)?,
                    chapter_title: ch.display_title(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// After a restart, anything that was mid-download goes back to the queue.
    pub fn reset_interrupted_downloads(&self) -> Result<()> {
        self.c().execute("UPDATE download SET state='queued' WHERE state='downloading'", [])?;
        Ok(())
    }

    // ---- backup support -------------------------------------------------

    pub fn category_names_for(&self, manga_id: i64) -> Result<Vec<String>> {
        let c = self.c();
        let mut s = c.prepare("SELECT c.name FROM category c JOIN manga_category mc ON mc.category_id=c.id WHERE mc.manga_id=?1")?;
        let v = s.query_map([manga_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn ensure_category(&self, name: &str) -> Result<i64> {
        let existing: Option<i64> = self.c().query_row("SELECT id FROM category WHERE name=?1", [name], |r| r.get(0)).optional()?;
        match existing {
            Some(id) => Ok(id),
            None => self.add_category(name),
        }
    }

    pub fn set_manga_categories(&self, manga_id: i64, categories: &[i64]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM manga_category WHERE manga_id=?1", [manga_id])?;
        for cat in categories {
            tx.execute("INSERT OR IGNORE INTO manga_category(manga_id,category_id) VALUES(?1,?2)", params![manga_id, cat])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn update_chapter_state(&self, chapter_id: i64, read: bool, last_page: i32, last_read_at: i64) -> Result<()> {
        self.c().execute(
            "UPDATE chapter SET read=?2, last_page=?3, last_read_at=?4 WHERE id=?1",
            params![chapter_id, read as i64, last_page, last_read_at],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manga(key: &str, title: &str) -> Manga {
        Manga { source: "test".into(), key: key.into(), title: title.into(), ..Default::default() }
    }

    fn chapters(n: usize) -> Vec<Chapter> {
        // newest first
        (0..n)
            .map(|i| Chapter {
                key: format!("c{}", n - i),
                title: format!("Chapter {}", n - i),
                number: Some((n - i) as f64),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn upsert_is_idempotent_and_keeps_library_flag() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        db.set_in_library(&[id], true).unwrap();
        let id2 = db.upsert_manga(&manga("a", "Alpha v2")).unwrap();
        assert_eq!(id, id2);
        let m = db.get_manga(id).unwrap().unwrap();
        assert_eq!(m.title, "Alpha v2");
        assert!(m.in_library);
        assert!(m.added_at > 0);
    }

    #[test]
    fn upsert_does_not_blank_existing_metadata() {
        let db = Db::open_memory().unwrap();
        let mut m = manga("a", "Alpha");
        m.description = "desc".into();
        let id = db.upsert_manga(&m).unwrap();
        m.description.clear();
        db.upsert_manga(&m).unwrap();
        assert_eq!(db.get_manga(id).unwrap().unwrap().description, "desc");
    }

    #[test]
    fn merge_flags_only_later_chapters_as_new() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        let first = db.merge_chapters(id, &chapters(3)).unwrap();
        assert!(first.is_empty(), "initial import must not flag new chapters");
        let second = db.merge_chapters(id, &chapters(5)).unwrap();
        assert_eq!(second.len(), 2);
        let chs = db.chapters(id).unwrap();
        assert_eq!(chs.len(), 5);
        assert_eq!(chs[0].key, "c5");
        assert_eq!(chs.iter().filter(|c| c.is_new).count(), 2);
        // re-merging adds nothing
        assert!(db.merge_chapters(id, &chapters(5)).unwrap().is_empty());
    }

    #[test]
    fn progress_marks_read_on_last_page() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        db.merge_chapters(id, &chapters(2)).unwrap();
        let ch = db.chapters(id).unwrap()[0].clone();
        db.set_progress(ch.id, 3, 10).unwrap();
        let c = db.get_chapter(ch.id).unwrap().unwrap();
        assert!(!c.read);
        assert_eq!(c.last_page, 3);
        db.set_progress(ch.id, 9, 10).unwrap();
        assert!(db.get_chapter(ch.id).unwrap().unwrap().read);
        db.set_read(&[ch.id], false).unwrap();
        let c = db.get_chapter(ch.id).unwrap().unwrap();
        assert!(!c.read);
        assert_eq!(c.last_page, 0);
    }

    #[test]
    fn library_filters_sorts_and_counts() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        let b = db.upsert_manga(&manga("b", "Beta")).unwrap();
        let _c = db.upsert_manga(&manga("c", "Not in library")).unwrap();
        db.set_in_library(&[a, b], true).unwrap();
        db.merge_chapters(a, &chapters(4)).unwrap();
        db.merge_chapters(b, &chapters(2)).unwrap();
        let ch = db.chapters(a).unwrap();
        db.set_read(&[ch[0].id, ch[1].id], true).unwrap();

        let all = db.library(&LibraryQuery::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].manga.title, "Alpha");
        assert_eq!(all[0].unread, 2);
        assert_eq!(all[0].total, 4);

        let q = LibraryQuery { search: "bet".into(), ..Default::default() };
        assert_eq!(db.library(&q).unwrap().len(), 1);
        let q = LibraryQuery { search: "100%".into(), ..Default::default() };
        assert!(db.library(&q).unwrap().is_empty());

        db.set_manga_read(&[b], true).unwrap();
        let q = LibraryQuery { sort: LibrarySort::Unread, descending: true, ..Default::default() };
        assert_eq!(db.library(&q).unwrap()[0].manga.title, "Alpha");
        let q = LibraryQuery { sort: LibrarySort::Unread, descending: false, ..Default::default() };
        assert_eq!(db.library(&q).unwrap()[0].manga.title, "Beta");
    }

    #[test]
    fn categories_and_moves() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        let b = db.upsert_manga(&manga("b", "Beta")).unwrap();
        db.set_in_library(&[a, b], true).unwrap();
        let cat = db.add_category("Favorites").unwrap();
        assert!(db.add_category("Favorites").is_err());
        db.move_to_category(&[a], Some(cat)).unwrap();
        let q = LibraryQuery { category: Some(cat), ..Default::default() };
        let got = db.library(&q).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].manga.id, a);
        let q = LibraryQuery { category: Some(0), ..Default::default() };
        assert_eq!(db.library(&q).unwrap()[0].manga.id, b);
        db.delete_category(cat).unwrap();
        assert_eq!(db.library(&LibraryQuery { category: Some(0), ..Default::default() }).unwrap().len(), 2);
    }

    #[test]
    fn removing_from_library_and_purge() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        let b = db.upsert_manga(&manga("b", "Beta")).unwrap();
        db.set_in_library(&[a, b], true).unwrap();
        db.merge_chapters(b, &chapters(2)).unwrap();
        let ch = db.chapters(b).unwrap()[0].id;
        db.set_progress(ch, 1, 10).unwrap();
        db.set_in_library(&[a, b], false).unwrap();
        db.purge_orphans().unwrap();
        assert!(db.get_manga(a).unwrap().is_none());
        assert!(db.get_manga(b).unwrap().is_some(), "manga with history survives");
    }

    #[test]
    fn history_lists_latest_chapter_per_manga() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        db.merge_chapters(a, &chapters(3)).unwrap();
        let ch = db.chapters(a).unwrap();
        assert!(db.history(10).unwrap().is_empty());
        db.set_progress(ch[2].id, 1, 10).unwrap();
        db.set_progress(ch[1].id, 2, 10).unwrap();
        let h = db.history(10).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].manga.title, "Alpha");
        db.clear_history().unwrap();
        assert!(db.history(10).unwrap().is_empty());
    }

    #[test]
    fn download_queue_roundtrip_and_restart_reset() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        db.merge_chapters(a, &chapters(2)).unwrap();
        let ch = db.chapters(a).unwrap();
        assert!(db.enqueue_download(ch[0].id).unwrap());
        assert!(!db.enqueue_download(ch[0].id).unwrap(), "already queued");
        db.set_download_state(ch[0].id, DownloadState::Downloading, "").unwrap();
        db.set_download_progress(ch[0].id, 3, 10).unwrap();
        db.reset_interrupted_downloads().unwrap();
        let jobs = db.download_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, DownloadState::Queued);
        assert_eq!(jobs[0].done_pages, 3);
        db.set_downloaded(ch[0].id, true, Some(10)).unwrap();
        assert!(!db.enqueue_download(ch[0].id).unwrap());
    }

    #[test]
    fn claim_only_takes_queued_jobs() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        db.merge_chapters(a, &chapters(1)).unwrap();
        let id = db.chapters(a).unwrap()[0].id;
        db.enqueue_download(id).unwrap();
        db.set_download_state(id, DownloadState::Paused, "").unwrap();
        assert!(!db.claim_download(id).unwrap(), "a paused job must not be started");
        db.enqueue_download(id).unwrap();
        assert!(db.claim_download(id).unwrap());
        assert!(!db.claim_download(id).unwrap(), "claimed twice");
        db.remove_download(id).unwrap();
        assert!(!db.claim_download(id).unwrap(), "cancelled job must not be started");
    }

    #[test]
    fn settings_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.load_settings(), Settings::default());
        let s = Settings { cache_limit_mb: 99, auto_download_new: true, ..Default::default() };
        db.save_settings(&s).unwrap();
        assert_eq!(db.load_settings(), s);
    }

    #[test]
    fn reader_override_roundtrip() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_manga(&manga("a", "Alpha")).unwrap();
        let ov = ReaderSettings { mode: ReaderMode::Webtoon, ..Default::default() };
        db.set_reader_override(a, Some(&ov)).unwrap();
        assert_eq!(db.get_manga(a).unwrap().unwrap().reader_override, Some(ov));
        db.set_reader_override(a, None).unwrap();
        assert!(db.get_manga(a).unwrap().unwrap().reader_override.is_none());
    }
}
