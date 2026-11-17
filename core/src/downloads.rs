//! Download queue: concurrent, pausable, resumable across restarts.
//!
//! The scheduler is event driven (a `Notify`), so an idle queue costs nothing.
//! Progress is persisted per page: a restart (or pause) resumes after the last
//! page already on disk.

use crate::db::Db;
use crate::models::{Chapter, DownloadState, Manga};
use crate::source::{Registry, Source};
use crate::util::sanitize_filename;
use anyhow::{anyhow, bail, Result};
use futures_util::stream::{self, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const CANCEL: u8 = 2;
/// Resume was requested while the job was still winding down after a pause.
const RESUME: u8 = 3;
const PAGE_CONCURRENCY: usize = 3;
const COMPLETE_MARKER: &str = ".complete";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEvent {
    /// Page progress for one chapter.
    Progress { chapter_id: i64, done: i32, total: i32 },
    /// The queue content or a job's state changed; reload the list.
    QueueChanged,
    /// A chapter finished (successfully or not).
    Finished { chapter_id: i64, ok: bool },
}

type Listener = Arc<dyn Fn(DownloadEvent) + Send + Sync>;

struct Config {
    root: PathBuf,
    max_concurrent: usize,
}

struct Inner {
    db: Db,
    registry: Registry,
    config: RwLock<Config>,
    active: Mutex<HashMap<i64, Arc<AtomicU8>>>,
    paused_all: Mutex<bool>,
    notify: Notify,
    listener: Listener,
}

#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<Inner>,
}

/// Directory holding one downloaded chapter.
///
/// The manga folder is named `Title [id]`. Titles can change when metadata is
/// refreshed, so an existing folder for the same id is reused whatever its title.
pub fn chapter_dir(root: &Path, manga: &Manga, chapter: &Chapter) -> PathBuf {
    let source_dir = root.join(sanitize_filename(&manga.source));
    let suffix = format!(" [{}]", manga.id);
    let existing = std::fs::read_dir(&source_dir).ok().and_then(|rd| {
        rd.flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(&suffix) && e.path().is_dir())
            .map(|e| e.path())
    });
    existing
        .unwrap_or_else(|| source_dir.join(format!("{}{suffix}", sanitize_filename(&manga.title))))
        .join(chapter.id.to_string())
}

/// Page files of a completed download, in reading order.
pub fn downloaded_pages(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.file_name().is_some_and(|n| !n.to_string_lossy().starts_with('.'))).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn is_complete(dir: &Path) -> bool {
    dir.join(COMPLETE_MARKER).exists()
}

fn sniff_ext(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xff, 0xd8, ..] => "jpg",
        [0x89, b'P', b'N', b'G', ..] => "png",
        [b'G', b'I', b'F', ..] => "gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "webp",
        [b'B', b'M', ..] => "bmp",
        _ if bytes.len() > 12 && &bytes[4..8] == b"ftyp" => "avif",
        _ => "img",
    }
}

fn page_exists(dir: &Path, index: usize) -> bool {
    let prefix = format!("{index:04}.");
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&prefix) && !e.file_name().to_string_lossy().ends_with(".part")))
        .unwrap_or(false)
}

impl DownloadManager {
    pub fn new(db: Db, registry: Registry, root: PathBuf, max_concurrent: usize, listener: Listener) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                registry,
                config: RwLock::new(Config { root, max_concurrent: max_concurrent.max(1) }),
                active: Mutex::new(HashMap::new()),
                paused_all: Mutex::new(false),
                notify: Notify::new(),
                listener,
            }),
        }
    }

    /// Spawn the scheduler on `rt`. Interrupted jobs from a previous run restart automatically.
    pub fn start(&self, rt: &tokio::runtime::Handle) {
        let _ = self.inner.db.reset_interrupted_downloads();
        let this = self.clone();
        rt.spawn(async move {
            loop {
                this.schedule().await;
                this.inner.notify.notified().await;
            }
        });
        self.inner.notify.notify_one();
    }

    pub fn root(&self) -> PathBuf {
        self.inner.config.read().map(|c| c.root.clone()).unwrap_or_default()
    }

    pub fn set_limits(&self, root: PathBuf, max_concurrent: usize) {
        if let Ok(mut c) = self.inner.config.write() {
            c.root = root;
            c.max_concurrent = max_concurrent.max(1);
        }
        self.inner.notify.notify_one();
    }

    fn emit(&self, e: DownloadEvent) {
        (self.inner.listener)(e);
    }

    /// Queue chapters. Chapters of the local source and already-downloaded ones are ignored.
    pub fn enqueue(&self, chapter_ids: &[i64]) -> Result<usize> {
        let mut queued = 0;
        for &id in chapter_ids {
            let Some(ch) = self.inner.db.get_chapter(id)? else { continue };
            let Some(m) = self.inner.db.get_manga(ch.manga_id)? else { continue };
            if m.source == "local" {
                continue;
            }
            if self.inner.db.enqueue_download(id)? {
                queued += 1;
            }
        }
        if queued > 0 {
            self.emit(DownloadEvent::QueueChanged);
            self.inner.notify.notify_one();
        }
        Ok(queued)
    }

    fn ctl(&self, id: i64) -> Option<Arc<AtomicU8>> {
        self.inner.active.lock().ok()?.get(&id).cloned()
    }

    pub fn pause(&self, id: i64) -> Result<()> {
        if let Some(c) = self.ctl(id) {
            c.store(PAUSE, Ordering::SeqCst);
        } else {
            self.inner.db.set_download_state(id, DownloadState::Paused, "")?;
            self.emit(DownloadEvent::QueueChanged);
        }
        Ok(())
    }

    /// Resume a paused or failed job.
    pub fn resume(&self, id: i64) -> Result<()> {
        match self.ctl(id) {
            // Still stopping after a pause: requeue it once it has stopped.
            Some(c) => {
                let _ = c.compare_exchange(PAUSE, RESUME, Ordering::SeqCst, Ordering::SeqCst);
            }
            None => self.inner.db.set_download_state(id, DownloadState::Queued, "")?,
        }
        self.emit(DownloadEvent::QueueChanged);
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Remove a job and its partial files.
    pub fn cancel(&self, id: i64) -> Result<()> {
        if let Some(c) = self.ctl(id) {
            c.store(CANCEL, Ordering::SeqCst);
            return Ok(());
        }
        self.discard(id)?;
        self.emit(DownloadEvent::QueueChanged);
        Ok(())
    }

    fn discard(&self, id: i64) -> Result<()> {
        if let Some(ch) = self.inner.db.get_chapter(id)? {
            if !ch.downloaded {
                if let Some(m) = self.inner.db.get_manga(ch.manga_id)? {
                    let _ = std::fs::remove_dir_all(chapter_dir(&self.root(), &m, &ch));
                }
            }
        }
        self.inner.db.remove_download(id)
    }

    pub fn pause_all(&self) {
        if let Ok(mut p) = self.inner.paused_all.lock() {
            *p = true;
        }
        if let Ok(a) = self.inner.active.lock() {
            for c in a.values() {
                c.store(PAUSE, Ordering::SeqCst);
            }
        }
        self.emit(DownloadEvent::QueueChanged);
    }

    pub fn resume_all(&self) -> Result<()> {
        if let Ok(mut p) = self.inner.paused_all.lock() {
            *p = false;
        }
        for j in self.inner.db.download_jobs()? {
            if matches!(j.state, DownloadState::Paused | DownloadState::Failed) {
                self.inner.db.set_download_state(j.chapter_id, DownloadState::Queued, "")?;
            }
        }
        self.emit(DownloadEvent::QueueChanged);
        self.inner.notify.notify_one();
        Ok(())
    }

    pub fn is_paused_all(&self) -> bool {
        self.inner.paused_all.lock().map(|p| *p).unwrap_or(false)
    }

    pub fn cancel_all(&self) -> Result<()> {
        for j in self.inner.db.download_jobs()? {
            self.cancel(j.chapter_id)?;
        }
        Ok(())
    }

    /// Delete downloaded files for chapters and clear their flags.
    pub fn delete_downloaded(&self, chapter_ids: &[i64]) -> Result<()> {
        for &id in chapter_ids {
            if let Some(ch) = self.inner.db.get_chapter(id)? {
                if let Some(m) = self.inner.db.get_manga(ch.manga_id)? {
                    let _ = std::fs::remove_dir_all(chapter_dir(&self.root(), &m, &ch));
                }
                self.inner.db.set_downloaded(id, false, None)?;
            }
        }
        self.emit(DownloadEvent::QueueChanged);
        Ok(())
    }

    async fn schedule(&self) {
        if self.is_paused_all() {
            return;
        }
        let max = self.inner.config.read().map(|c| c.max_concurrent).unwrap_or(1);
        let Ok(jobs) = self.inner.db.download_jobs() else { return };
        for job in jobs {
            if job.state != DownloadState::Queued {
                continue;
            }
            let ctl = {
                let Ok(mut active) = self.inner.active.lock() else { return };
                if active.len() >= max {
                    break;
                }
                if active.contains_key(&job.chapter_id) {
                    continue;
                }
                let c = Arc::new(AtomicU8::new(RUN));
                active.insert(job.chapter_id, c.clone());
                c
            };
            // Paused or cancelled between listing and now: leave it alone.
            if !self.inner.db.claim_download(job.chapter_id).unwrap_or(false) {
                if let Ok(mut a) = self.inner.active.lock() {
                    a.remove(&job.chapter_id);
                }
                continue;
            }
            self.emit(DownloadEvent::QueueChanged);
            let this = self.clone();
            tokio::spawn(async move {
                let id = job.chapter_id;
                let result = this.run_job(id, &ctl).await;
                if let Ok(mut a) = this.inner.active.lock() {
                    a.remove(&id);
                }
                let ok = match result {
                    Ok(Outcome::Done) => true,
                    Ok(Outcome::Paused) => {
                        let _ = this.inner.db.set_download_state(id, DownloadState::Paused, "");
                        false
                    }
                    Ok(Outcome::Requeued) => {
                        let _ = this.inner.db.set_download_state(id, DownloadState::Queued, "");
                        false
                    }
                    Ok(Outcome::Cancelled) => {
                        let _ = this.discard(id);
                        false
                    }
                    Err(e) => {
                        let _ = this.inner.db.set_download_state(id, DownloadState::Failed, &e.to_string());
                        false
                    }
                };
                this.emit(DownloadEvent::Finished { chapter_id: id, ok });
                this.emit(DownloadEvent::QueueChanged);
                this.inner.notify.notify_one();
            });
        }
    }

    async fn run_job(&self, id: i64, ctl: &Arc<AtomicU8>) -> Result<Outcome> {
        let db = &self.inner.db;
        let ch = db.get_chapter(id)?.ok_or_else(|| anyhow!("chapter vanished"))?;
        let manga = db.get_manga(ch.manga_id)?.ok_or_else(|| anyhow!("manga vanished"))?;
        let src: Arc<dyn Source> = self.inner.registry.get(&manga.source).ok_or_else(|| anyhow!("unknown source {}", manga.source))?;
        let dir = chapter_dir(&self.root(), &manga, &ch);
        std::fs::create_dir_all(&dir)?;
        let pages = src.pages(&ch.key).await?;
        let total = pages.len() as i32;
        if total == 0 {
            bail!("chapter has no pages");
        }
        let done = Arc::new(std::sync::atomic::AtomicI32::new((0..pages.len()).filter(|&i| page_exists(&dir, i)).count() as i32));
        db.set_download_progress(id, done.load(Ordering::SeqCst), total)?;

        let results: Vec<Result<bool>> = stream::iter(pages.into_iter().enumerate())
            .map(|(i, page)| {
                let (src, dir, ctl, done, this, key) = (src.clone(), dir.clone(), ctl.clone(), done.clone(), self.clone(), ch.key.clone());
                async move {
                    if ctl.load(Ordering::SeqCst) != RUN {
                        return Ok(false);
                    }
                    if page_exists(&dir, i) {
                        return Ok(true);
                    }
                    let mut last = anyhow!("download failed");
                    for attempt in 0..3u64 {
                        match src.image(&key, &page).await {
                            Ok(bytes) => {
                                let final_path = dir.join(format!("{i:04}.{}", sniff_ext(&bytes)));
                                let part = dir.join(format!("{i:04}.part"));
                                std::fs::write(&part, &bytes)?;
                                std::fs::rename(&part, &final_path)?;
                                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                                let _ = this.inner.db.set_download_progress(id, n, total);
                                this.emit(DownloadEvent::Progress { chapter_id: id, done: n, total });
                                return Ok(true);
                            }
                            Err(e) => {
                                last = e;
                                if ctl.load(Ordering::SeqCst) != RUN {
                                    return Ok(false);
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
                            }
                        }
                    }
                    Err(last)
                }
            })
            .buffer_unordered(PAGE_CONCURRENCY)
            .collect()
            .await;

        match ctl.load(Ordering::SeqCst) {
            CANCEL => return Ok(Outcome::Cancelled),
            PAUSE => return Ok(Outcome::Paused),
            RESUME => return Ok(Outcome::Requeued),
            _ => {}
        }
        let mut stored = 0;
        for r in results {
            if r? {
                stored += 1;
            }
        }
        // Never mark a chapter complete with pages missing.
        if stored != total {
            bail!("only {stored} of {total} pages were saved");
        }
        std::fs::write(dir.join(COMPLETE_MARKER), total.to_string())?;
        db.set_downloaded(id, true, Some(total))?;
        db.remove_download(id)?;
        Ok(Outcome::Done)
    }
}

enum Outcome {
    Done,
    Paused,
    Cancelled,
    Requeued,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSource;
    use std::time::Duration;
    use tokio::sync::mpsc;

    struct Rig {
        db: Db,
        mgr: DownloadManager,
        fake: Arc<FakeSource>,
        rx: mpsc::UnboundedReceiver<DownloadEvent>,
        dir: tempfile::TempDir,
        chapters: Vec<Chapter>,
    }

    fn rig(chapters: usize, pages: usize, delay_ms: u64, max: usize) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeSource::new(chapters, pages);
        fake.delay = Duration::from_millis(delay_ms);
        let fake = Arc::new(fake);
        let db = Db::open_memory().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let mgr = DownloadManager::new(db.clone(), Registry::new(vec![fake.clone()]), dir.path().join("dl"), max, Arc::new(move |e| {
            let _ = tx.send(e);
        }));
        let id = db.upsert_manga(&Manga { source: "fake".into(), key: "m".into(), title: "Title: Odd/Name".into(), ..Default::default() }).unwrap();
        let list: Vec<Chapter> = FakeSource::list(chapters).into_iter().map(|c| Chapter { key: c.key, title: c.title, number: c.number, ..Default::default() }).collect();
        db.merge_chapters(id, &list).unwrap();
        let chapters = db.chapters(id).unwrap();
        Rig { db, mgr, fake, rx, dir, chapters }
    }

    async fn wait_finished(rx: &mut mpsc::UnboundedReceiver<DownloadEvent>, n: usize) -> Vec<(i64, bool)> {
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            while out.len() < n {
                if let Some(DownloadEvent::Finished { chapter_id, ok }) = rx.recv().await {
                    out.push((chapter_id, ok));
                }
            }
        })
        .await
        .expect("timed out waiting for downloads");
        out
    }

    #[tokio::test]
    async fn downloads_chapters_and_marks_them_offline() {
        let mut r = rig(2, 3, 0, 2);
        r.mgr.start(&tokio::runtime::Handle::current());
        let ids: Vec<i64> = r.chapters.iter().map(|c| c.id).collect();
        assert_eq!(r.mgr.enqueue(&ids).unwrap(), 2);
        assert_eq!(r.mgr.enqueue(&ids).unwrap(), 0, "no duplicates");
        let fin = wait_finished(&mut r.rx, 2).await;
        assert!(fin.iter().all(|f| f.1));
        let m = r.db.get_manga(r.chapters[0].manga_id).unwrap().unwrap();
        for id in ids {
            let ch = r.db.get_chapter(id).unwrap().unwrap();
            assert!(ch.downloaded);
            assert_eq!(ch.page_count, 3);
            let dir = chapter_dir(&r.mgr.root(), &m, &ch);
            assert!(is_complete(&dir));
            let pages = downloaded_pages(&dir);
            assert_eq!(pages.len(), 3);
            assert!(pages[0].to_string_lossy().ends_with("0000.png"));
        }
        assert!(r.db.download_jobs().unwrap().is_empty());
        let _ = &r.dir;
    }

    #[tokio::test]
    async fn respects_concurrency_limit() {
        let mut r = rig(4, 2, 60, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let ids: Vec<i64> = r.chapters.iter().map(|c| c.id).collect();
        r.mgr.enqueue(&ids).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let downloading = r.db.download_jobs().unwrap().iter().filter(|j| j.state == DownloadState::Downloading).count();
        assert_eq!(downloading, 1);
        wait_finished(&mut r.rx, 4).await;
    }

    #[tokio::test]
    async fn pause_then_resume_continues_without_refetching() {
        let mut r = rig(1, 6, 80, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let id = r.chapters[0].id;
        r.mgr.enqueue(&[id]).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        r.mgr.pause(id).unwrap();
        let fin = wait_finished(&mut r.rx, 1).await;
        assert!(!fin[0].1);
        let jobs = r.db.download_jobs().unwrap();
        assert_eq!(jobs[0].state, DownloadState::Paused);
        let fetched_before = r.fake.image_calls.load(Ordering::SeqCst);
        assert!(fetched_before < 6 + 3);
        assert!(!r.db.get_chapter(id).unwrap().unwrap().downloaded);

        r.mgr.resume(id).unwrap();
        let fin = wait_finished(&mut r.rx, 1).await;
        assert!(fin[0].1);
        assert!(r.db.get_chapter(id).unwrap().unwrap().downloaded);
        let total_calls = r.fake.image_calls.load(Ordering::SeqCst);
        assert!(total_calls <= 6 + PAGE_CONCURRENCY, "pages already on disk must not be refetched ({total_calls})");
    }

    #[tokio::test]
    async fn cancel_removes_job_and_files() {
        let mut r = rig(1, 6, 80, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let id = r.chapters[0].id;
        r.mgr.enqueue(&[id]).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        r.mgr.cancel(id).unwrap();
        wait_finished(&mut r.rx, 1).await;
        assert!(r.db.download_jobs().unwrap().is_empty());
        let m = r.db.get_manga(r.chapters[0].manga_id).unwrap().unwrap();
        let ch = r.db.get_chapter(id).unwrap().unwrap();
        assert!(!chapter_dir(&r.mgr.root(), &m, &ch).exists());
        assert!(!ch.downloaded);
    }

    #[tokio::test]
    async fn queued_jobs_survive_restart() {
        let mut r = rig(1, 2, 0, 1);
        let id = r.chapters[0].id;
        // job was mid-download when the app died
        r.db.enqueue_download(id).unwrap();
        r.db.set_download_state(id, DownloadState::Downloading, "").unwrap();
        r.mgr.start(&tokio::runtime::Handle::current());
        let fin = wait_finished(&mut r.rx, 1).await;
        assert_eq!(fin, vec![(id, true)]);
    }

    #[tokio::test]
    async fn pause_all_holds_queue_until_resume_all() {
        let mut r = rig(2, 1, 0, 2);
        r.mgr.pause_all();
        r.mgr.start(&tokio::runtime::Handle::current());
        let ids: Vec<i64> = r.chapters.iter().map(|c| c.id).collect();
        r.mgr.enqueue(&ids).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(r.fake.image_calls.load(Ordering::SeqCst), 0);
        r.mgr.resume_all().unwrap();
        wait_finished(&mut r.rx, 2).await;
    }

    #[tokio::test]
    async fn delete_downloaded_clears_files_and_flag() {
        let mut r = rig(1, 2, 0, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let id = r.chapters[0].id;
        r.mgr.enqueue(&[id]).unwrap();
        wait_finished(&mut r.rx, 1).await;
        r.mgr.delete_downloaded(&[id]).unwrap();
        let ch = r.db.get_chapter(id).unwrap().unwrap();
        assert!(!ch.downloaded);
        let m = r.db.get_manga(ch.manga_id).unwrap().unwrap();
        assert!(!chapter_dir(&r.mgr.root(), &m, &ch).exists());
    }

    #[tokio::test]
    async fn resume_right_after_pause_is_not_lost() {
        let mut r = rig(1, 6, 80, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let id = r.chapters[0].id;
        r.mgr.enqueue(&[id]).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        r.mgr.pause(id).unwrap();
        r.mgr.resume(id).unwrap(); // job is still stopping
        let mut fin = wait_finished(&mut r.rx, 1).await;
        if !fin[0].1 {
            fin = wait_finished(&mut r.rx, 1).await;
        }
        assert!(fin[0].1, "resumed job must finish");
        assert!(r.db.get_chapter(id).unwrap().unwrap().downloaded);
    }

    #[tokio::test]
    async fn renamed_manga_keeps_its_download_folder() {
        let mut r = rig(1, 2, 0, 1);
        r.mgr.start(&tokio::runtime::Handle::current());
        let id = r.chapters[0].id;
        r.mgr.enqueue(&[id]).unwrap();
        wait_finished(&mut r.rx, 1).await;
        let mut m = r.db.get_manga(r.chapters[0].manga_id).unwrap().unwrap();
        let ch = r.db.get_chapter(id).unwrap().unwrap();
        let before = chapter_dir(&r.mgr.root(), &m, &ch);
        m.title = "A completely new title".into();
        r.db.upsert_manga(&m).unwrap();
        let m = r.db.get_manga(m.id).unwrap().unwrap();
        let after = chapter_dir(&r.mgr.root(), &m, &ch);
        assert_eq!(before, after);
        assert!(is_complete(&after));
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff_ext(&[0xff, 0xd8, 0xff]), "jpg");
        assert_eq!(sniff_ext(b"RIFF\0\0\0\0WEBPVP8"), "webp");
        assert_eq!(sniff_ext(b"???"), "img");
    }
}
