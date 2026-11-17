//! Library update checks.

use crate::db::Db;
use crate::models::Manga;
use crate::source::Registry;
use anyhow::{anyhow, Result};
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;

#[derive(Debug, Default, Clone)]
pub struct UpdateReport {
    pub checked: usize,
    /// Ids of chapters found that were not known before.
    pub new_chapter_ids: Vec<i64>,
    /// (manga title, error)
    pub failures: Vec<(String, String)>,
}

fn to_chapters(list: Vec<crate::source::ChapterEntry>) -> Vec<crate::models::Chapter> {
    list.into_iter()
        .map(|c| crate::models::Chapter {
            key: c.key,
            title: c.title,
            number: c.number,
            volume: c.volume,
            scanlator: c.scanlator,
            published: c.published,
            ..Default::default()
        })
        .collect()
}

/// Fetch details and chapters for a manga and persist them. Returns the manga id and new chapter ids.
pub async fn sync_manga(db: &Db, registry: &Registry, source_id: &str, key: &str) -> Result<(i64, Vec<i64>)> {
    let src = registry.get(source_id).ok_or_else(|| anyhow!("unknown source {source_id}"))?;
    let details = src.details(key).await?;
    let id = db.upsert_manga(&Manga {
        source: source_id.into(),
        key: key.into(),
        title: details.title,
        cover_url: details.cover_url,
        description: details.description,
        authors: details.authors,
        genres: details.genres,
        status: details.status,
        ..Default::default()
    })?;
    let chapters = src.chapters(key).await?;
    let new_ids = db.merge_chapters(id, &to_chapters(chapters))?;
    Ok((id, new_ids))
}

/// Refresh only the chapter list of one stored manga.
pub async fn refresh_chapters(db: &Db, registry: &Registry, manga_id: i64) -> Result<Vec<i64>> {
    let m = db.get_manga(manga_id)?.ok_or_else(|| anyhow!("manga {manga_id} not found"))?;
    let src = registry.get(&m.source).ok_or_else(|| anyhow!("unknown source {}", m.source))?;
    let chapters = src.chapters(&m.key).await?;
    db.merge_chapters(manga_id, &to_chapters(chapters))
}

/// Check manga for new chapters (3 at a time). `progress(done, total)` is called after each.
pub async fn check_library(db: &Db, registry: &Registry, ids: Vec<i64>, progress: Arc<dyn Fn(usize, usize) + Send + Sync>) -> UpdateReport {
    let total = ids.len();
    let mut report = UpdateReport::default();
    let mut results = stream::iter(ids)
        .map(|id| {
            let (db, registry) = (db.clone(), registry.clone());
            async move {
                let title = db.get_manga(id).ok().flatten().map(|m| m.title).unwrap_or_default();
                (title, refresh_chapters(&db, &registry, id).await)
            }
        })
        .buffer_unordered(3);
    let mut done = 0;
    while let Some((title, res)) = results.next().await {
        done += 1;
        report.checked += 1;
        match res {
            Ok(mut new) => report.new_chapter_ids.append(&mut new),
            Err(e) => report.failures.push((title, e.to_string())),
        }
        progress(done, total);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn setup(n: usize) -> (Db, Registry, Arc<FakeSource>) {
        let fake = Arc::new(FakeSource::new(n, 2));
        let reg = Registry::new(vec![fake.clone()]);
        (Db::open_memory().unwrap(), reg, fake)
    }

    #[tokio::test]
    async fn sync_then_update_finds_only_new_chapters() {
        let (db, reg, fake) = setup(3);
        let (id, new) = sync_manga(&db, &reg, "fake", "m1").await.unwrap();
        assert!(new.is_empty());
        assert_eq!(db.chapters(id).unwrap().len(), 3);
        db.set_in_library(&[id], true).unwrap();

        *fake.chapters.lock().unwrap() = FakeSource::list(5);
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let report = check_library(&db, &reg, vec![id], Arc::new(move |_, _| { c2.fetch_add(1, Ordering::SeqCst); })).await;
        assert_eq!(report.checked, 1);
        assert_eq!(report.new_chapter_ids.len(), 2);
        assert!(report.failures.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let entry = &db.library(&Default::default()).unwrap()[0];
        assert_eq!(entry.new_count, 2);
        assert_eq!(entry.unread, 5);
    }

    #[tokio::test]
    async fn failures_are_collected_not_fatal() {
        let (db, reg, fake) = setup(2);
        let (id, _) = sync_manga(&db, &reg, "fake", "m1").await.unwrap();
        *fake.fail_chapters.lock().unwrap() = true;
        let report = check_library(&db, &reg, vec![id], Arc::new(|_, _| {})).await;
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].1.contains("boom"));
        assert!(report.new_chapter_ids.is_empty());
    }
}
