//! History, Updates and Downloads views, plus library update checks.

use crate::ctx::{set_model, to_image, C};
use slint::{ComponentHandle, Model};
use std::sync::Arc;
use tankobon_core::downloads::DownloadEvent;
use tankobon_core::util::{fmt_ago, now};
use tankobon_core::{DownloadState, HistoryEntry};
use tankobon_ui::{Actions, DownloadRow, ListRow, State};

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    {
        let c = ctx.clone();
        a.on_show_view(move |v| {
            if let Some(w) = c.win() {
                w.global::<State>().set_view(v);
            }
            match v {
                0 => crate::library::refresh(&c),
                2 => refresh_updates(&c),
                3 => refresh_history(&c),
                4 => refresh_downloads(&c),
                _ => {}
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_dismiss_toast(move || {
            if let Some(w) = c.win() {
                w.global::<State>().set_toast("".into());
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_need_list_cover(move |manga_id| need_cover(&c, manga_id));
    }
    {
        let c = ctx.clone();
        a.on_history_open(move |id| crate::reader::open_chapter(&c, id as i64, None));
    }
    {
        let c = ctx.clone();
        a.on_updates_open(move |id| crate::reader::open_chapter(&c, id as i64, None));
    }
    {
        let c = ctx.clone();
        a.on_history_clear(move || {
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let _ = c2.db.clear_history();
                let c3 = c2.clone();
                c2.ui(move |_| refresh_history(&c3));
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_updates_mark_seen(move || {
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let _ = c2.db.clear_all_new();
                let c3 = c2.clone();
                c2.ui(move |_| {
                    refresh_updates(&c3);
                    crate::library::refresh(&c3);
                });
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_update_library(move || update_library(&c, false));
    }
    {
        let c = ctx.clone();
        a.on_download_pause(move |id| {
            let d = c.downloads.clone();
            c.rt.spawn_blocking(move || d.pause(id as i64));
        });
    }
    {
        let c = ctx.clone();
        a.on_download_resume(move |id| {
            let d = c.downloads.clone();
            c.rt.spawn_blocking(move || d.resume(id as i64));
        });
    }
    {
        let c = ctx.clone();
        a.on_download_cancel(move |id| {
            // removes partial files: keep disk I/O off the UI thread
            let d = c.downloads.clone();
            c.rt.spawn_blocking(move || d.cancel(id as i64));
        });
    }
    {
        let c = ctx.clone();
        a.on_downloads_pause_all(move || {
            c.downloads.pause_all();
            if let Some(w) = c.win() {
                w.global::<State>().set_downloads_paused(true);
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_downloads_resume_all(move || {
            let d = c.downloads.clone();
            c.rt.spawn_blocking(move || d.resume_all());
            if let Some(w) = c.win() {
                w.global::<State>().set_downloads_paused(false);
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_downloads_cancel_all(move || {
            let d = c.downloads.clone();
            c.rt.spawn_blocking(move || d.cancel_all());
        });
    }
}

fn row_of(e: &HistoryEntry, subtitle: String, when: String) -> ListRow {
    ListRow {
        chapter_id: e.chapter.id as i32,
        manga_id: e.manga.id as i32,
        title: e.manga.title.clone().into(),
        subtitle: subtitle.into(),
        when: when.into(),
        cover: Default::default(),
        has_cover: false,
    }
}

fn keep_covers(old: &slint::ModelRc<ListRow>, rows: &mut [ListRow]) {
    for r in rows.iter_mut() {
        if let Some(o) = old.iter().find(|o| o.manga_id == r.manga_id && o.has_cover) {
            r.cover = o.cover.clone();
            r.has_cover = true;
        }
    }
}

pub fn refresh_history(ctx: &C) {
    ctx.blocking(
        |c| c.db.history(200).unwrap_or_default(),
        |w, entries| {
            let t = now();
            let mut rows: Vec<ListRow> = entries
                .iter()
                .map(|e| {
                    let mut sub = e.chapter.display_title();
                    if !e.chapter.read && e.chapter.page_count > 0 {
                        sub.push_str(&format!(" — page {}/{}", e.chapter.last_page + 1, e.chapter.page_count));
                    }
                    row_of(e, sub, fmt_ago(e.chapter.last_read_at, t))
                })
                .collect();
            let st = w.global::<State>();
            keep_covers(&st.get_history(), &mut rows);
            if let Some(m) = set_model(st.get_history(), rows) {
                st.set_history(m);
            }
        },
    );
}

pub fn refresh_updates(ctx: &C) {
    ctx.blocking(
        |c| c.db.recent_new_chapters(200).unwrap_or_default(),
        |w, entries| {
            let t = now();
            let mut rows: Vec<ListRow> = entries.iter().map(|e| row_of(e, e.chapter.display_title(), format!("found {}", fmt_ago(e.chapter.found_at, t)))).collect();
            let st = w.global::<State>();
            keep_covers(&st.get_updates(), &mut rows);
            if let Some(m) = set_model(st.get_updates(), rows) {
                st.set_updates(m);
            }
        },
    );
}

pub fn refresh_downloads(ctx: &C) {
    let paused = ctx.downloads.is_paused_all();
    ctx.blocking(
        |c| c.db.download_jobs().unwrap_or_default(),
        move |w, jobs| {
            let rows: Vec<DownloadRow> = jobs
                .iter()
                .map(|j| DownloadRow {
                    chapter_id: j.chapter_id as i32,
                    title: j.manga_title.clone().into(),
                    subtitle: if j.error.is_empty() { j.chapter_title.clone() } else { format!("{} — {}", j.chapter_title, j.error) }.into(),
                    state: match j.state {
                        DownloadState::Downloading => format!("{}/{}", j.done_pages, j.total_pages),
                        DownloadState::Queued => "Queued".into(),
                        DownloadState::Paused => "Paused".into(),
                        DownloadState::Failed => "Failed".into(),
                        DownloadState::Done => "Done".into(),
                    }
                    .into(),
                    progress: if j.total_pages > 0 { j.done_pages as f32 / j.total_pages as f32 } else { 0.0 },
                    paused: j.state == DownloadState::Paused,
                    failed: j.state == DownloadState::Failed,
                })
                .collect();
            let st = w.global::<State>();
            st.set_downloads_paused(paused);
            if let Some(m) = set_model(st.get_downloads(), rows) {
                st.set_downloads(m);
            }
        },
    );
}

fn need_cover(ctx: &C, manga_id: i32) {
    let Some(w) = ctx.win() else { return };
    let st = w.global::<State>();
    let missing = |m: slint::ModelRc<ListRow>| m.iter().any(|r| r.manga_id == manga_id && !r.has_cover);
    if !missing(st.get_history()) && !missing(st.get_updates()) {
        return;
    }
    let key = format!("list:{manga_id}");
    if !ctx.app().covers_inflight.insert(key.clone()) {
        return;
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let c2 = c.clone();
        let manga = tokio::task::spawn_blocking(move || c2.db.get_manga(manga_id as i64).ok().flatten()).await.ok().flatten();
        let buf = match manga {
            Some(m) => crate::covers::load(&c, &m.source, &m.cover_url).await,
            None => None,
        };
        c.app().covers_inflight.remove(&key);
        if let Some(buf) = buf {
            c.ui(move |w| {
                let img = to_image(buf);
                let st = w.global::<State>();
                for m in [st.get_history(), st.get_updates()] {
                    for i in 0..m.row_count() {
                        if let Some(mut r) = m.row_data(i) {
                            if r.manga_id == manga_id && !r.has_cover {
                                r.cover = img.clone();
                                r.has_cover = true;
                                m.set_row_data(i, r);
                            }
                        }
                    }
                }
            });
        }
    });
}

/// Download manager callback; may run on any thread.
pub fn on_download_event(ctx: &C, e: DownloadEvent) {
    match e {
        DownloadEvent::Progress { chapter_id, done, total } => {
            let p = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            let c = ctx.clone();
            ctx.ui(move |w| {
                let st = w.global::<State>();
                {
                    let mut a = c.app();
                    if a.manga.id != 0 {
                        a.manga.dl.insert(chapter_id, (2, p));
                    }
                }
                let m = st.get_chapters();
                for i in 0..m.row_count() {
                    if let Some(mut r) = m.row_data(i) {
                        if r.id as i64 == chapter_id {
                            r.dl_state = 2;
                            r.progress = p;
                            m.set_row_data(i, r);
                            break;
                        }
                    }
                }
                let d = st.get_downloads();
                for i in 0..d.row_count() {
                    if let Some(mut r) = d.row_data(i) {
                        if r.chapter_id as i64 == chapter_id {
                            r.progress = p;
                            r.state = format!("{done}/{total}").into();
                            d.set_row_data(i, r);
                            break;
                        }
                    }
                }
            });
        }
        DownloadEvent::QueueChanged | DownloadEvent::Finished { .. } => {
            let c = ctx.clone();
            ctx.ui(move |_| {
                refresh_downloads(&c);
                crate::manga::reload(&c);
            });
        }
    }
}

/// Check all library manga for new chapters (UI thread).
pub fn update_library(ctx: &C, quiet: bool) {
    {
        let mut a = ctx.app();
        if a.update_running {
            return;
        }
        a.update_running = true;
    }
    if let Some(w) = ctx.win() {
        let st = w.global::<State>();
        st.set_updating(true);
        st.set_update_status("Starting…".into());
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let ids = c.db.library_ids().unwrap_or_default();
        let total = ids.len();
        let c2 = c.clone();
        let progress: Arc<dyn Fn(usize, usize) + Send + Sync> = Arc::new(move |done, total| {
            c2.ui(move |w| w.global::<State>().set_update_status(format!("Checked {done}/{total}").into()));
        });
        let report = tankobon_core::updates::check_library(&c.db, &c.registry, ids, progress).await;
        if c.settings().auto_download_new && !report.new_chapter_ids.is_empty() {
            let _ = c.downloads.enqueue(&report.new_chapter_ids);
        }
        c.app().update_running = false;
        let n = report.new_chapter_ids.len();
        let failed = report.failures.len();
        let status = if total == 0 {
            "Library is empty".to_string()
        } else if failed > 0 {
            format!("{n} new chapter(s); {failed} failed ({})", report.failures[0].0)
        } else {
            format!("{n} new chapter(s)")
        };
        let c3 = c.clone();
        c.ui(move |w| {
            let st = w.global::<State>();
            st.set_updating(false);
            st.set_update_status(status.clone().into());
            refresh_updates(&c3);
            crate::library::refresh(&c3);
            if !quiet || n > 0 {
                c3.toast(status);
            }
        });
    });
}
