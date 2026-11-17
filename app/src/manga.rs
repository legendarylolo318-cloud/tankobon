//! Manga detail page: metadata, chapter list, reading entry points.

use crate::ctx::{set_model, to_image, C};
use slint::ComponentHandle;
use tankobon_core::util::{fmt_date, now};
use tankobon_core::{Chapter, DownloadState};
use tankobon_ui::{Actions, ChapterRow, State};

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    macro_rules! on {
        ($m:ident, $f:expr) => {{
            let c = ctx.clone();
            a.$m(move || $f(&c));
        }};
    }
    on!(on_close_manga, close);
    on!(on_toggle_library, toggle_library);
    on!(on_refresh_manga, |c: &C| sync(c, true));
    on!(on_resume_reading, resume);
    on!(on_chapters_select_all, |c: &C| {
        let ids: Vec<i64> = c.app().manga.chapters.iter().map(|x| x.id).collect();
        c.app().manga.selected = ids.into_iter().collect();
        render_rows(c);
    });
    on!(on_chapters_clear_selection, |c: &C| {
        c.app().manga.selected.clear();
        render_rows(c);
    });
    on!(on_chapters_sort_changed, render_rows);
    on!(on_chapters_mark_previous_read, mark_previous_read);
    on!(on_chapters_download, |c: &C| {
        let ids = selected(c);
        enqueue(c, ids);
        c.app().manga.selected.clear();
        render_rows(c);
    });
    on!(on_chapters_delete_downloads, |c: &C| {
        let ids = selected(c);
        let c2 = c.clone();
        c.rt.spawn_blocking(move || {
            let _ = c2.downloads.delete_downloaded(&ids);
            let c3 = c2.clone();
            c2.ui(move |_| reload(&c3));
        });
        c.app().manga.selected.clear();
    });
    on!(on_download_all_unread, |c: &C| {
        let id = c.app().manga.id;
        let c2 = c.clone();
        c.rt.spawn_blocking(move || {
            let ids = c2.db.undownloaded_unread(&[id]).unwrap_or_default();
            enqueue(&c2, ids);
        });
    });
    {
        let c = ctx.clone();
        a.on_chapter_open(move |id| crate::reader::open_chapter(&c, id as i64, None));
    }
    {
        let c = ctx.clone();
        a.on_chapter_toggle_select(move |id| {
            {
                let mut app = c.app();
                let id = id as i64;
                if !app.manga.selected.remove(&id) {
                    app.manga.selected.insert(id);
                }
            }
            render_rows(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_chapter_download_one(move |id| enqueue(&c, vec![id as i64]));
    }
    {
        let c = ctx.clone();
        a.on_chapters_mark_read(move |read| {
            let ids = selected(&c);
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let _ = c2.db.set_read(&ids, read);
                let c3 = c2.clone();
                c2.ui(move |_| reload(&c3));
            });
            c.app().manga.selected.clear();
        });
    }
}

fn selected(ctx: &C) -> Vec<i64> {
    ctx.app().manga.selected.iter().copied().collect()
}

/// Queue chapters for download (database work runs off the UI thread).
fn enqueue(ctx: &C, ids: Vec<i64>) {
    let c = ctx.clone();
    ctx.rt.spawn_blocking(move || match c.downloads.enqueue(&ids) {
        Ok(0) => c.toast("Nothing new to download"),
        Ok(n) => c.toast(format!("Queued {n} chapter(s)")),
        Err(e) => c.toast(format!("Download failed: {e}")),
    });
}

/// Open the manga page (UI thread).
pub fn open(ctx: &C, id: i64) {
    let Some(w) = ctx.win() else { return };
    {
        let mut a = ctx.app();
        a.manga = Default::default();
        a.manga.id = id;
    }
    let st = w.global::<State>();
    st.set_m_id(id as i32);
    st.set_m_title("".into());
    st.set_m_has_cover(false);
    st.set_m_description("".into());
    st.set_m_error("".into());
    st.set_chapter_selected_count(0);
    if let Some(m) = set_model(st.get_chapters(), vec![]) {
        st.set_chapters(m);
    }
    st.set_overlay(1);
    reload(ctx);
    // New (not yet fetched) manga and local sources sync right away.
    sync(ctx, false);
}

pub fn close(ctx: &C) {
    if let Some(w) = ctx.win() {
        w.global::<State>().set_overlay(0);
    }
    ctx.app().manga = Default::default();
    crate::library::refresh(ctx);
    crate::lists::refresh_history(ctx);
}

/// Reload manga + chapters from the database if the page is open (UI thread).
pub fn reload(ctx: &C) {
    let id = ctx.app().manga.id;
    if id == 0 {
        return;
    }
    let gen = {
        let mut a = ctx.app();
        a.manga.gen += 1;
        a.manga.gen
    };
    let c = ctx.clone();
    ctx.blocking(
        move |c| {
            let manga = c.db.get_manga(id).ok().flatten();
            let chapters = c.db.chapters(id).unwrap_or_default();
            let jobs = c.db.download_jobs().unwrap_or_default();
            (manga, chapters, jobs)
        },
        move |w, (manga, chapters, jobs)| {
            if c.app().manga.id != id || c.app().manga.gen != gen {
                return;
            }
            let Some(m) = manga else { return };
            let st = w.global::<State>();
            st.set_m_title(m.title.clone().into());
            st.set_m_description(m.description.clone().into());
            st.set_m_authors(m.authors.clone().into());
            st.set_m_genres(m.genres.join(" · ").into());
            st.set_m_status(m.status.label().into());
            st.set_m_source(c.registry.get(&m.source).map(|s| s.name().to_string()).unwrap_or(m.source.clone()).into());
            st.set_m_in_library(m.in_library);
            st.set_m_local(m.source == "local");
            {
                let mut a = c.app();
                a.manga.chapters = chapters;
                a.manga.dl = jobs
                    .iter()
                    .map(|j| {
                        let code = match j.state {
                            DownloadState::Queued => 1,
                            DownloadState::Downloading => 2,
                            DownloadState::Paused => 3,
                            _ => 4,
                        };
                        let p = if j.total_pages > 0 { j.done_pages as f32 / j.total_pages as f32 } else { 0.0 };
                        (j.chapter_id, (code, p))
                    })
                    .collect();
            }
            render_rows(&c);
            if !st.get_m_has_cover() {
                load_cover(&c, m.source, m.cover_url);
            }
        },
    );
}

fn load_cover(ctx: &C, source: String, url: String) {
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        if let Some(buf) = crate::covers::load(&c, &source, &url).await {
            c.ui(move |w| {
                let st = w.global::<State>();
                st.set_m_cover(to_image(buf));
                st.set_m_has_cover(true);
            });
        }
    });
}

/// Rebuild the chapter list model from controller state (UI thread).
pub fn render_rows(ctx: &C) {
    let Some(w) = ctx.win() else { return };
    let st = w.global::<State>();
    let desc = st.get_chapters_desc();
    let hide_read = st.get_hide_read();
    let (rows, n_selected, label) = {
        let a = ctx.app();
        let mut list: Vec<&Chapter> = a.manga.chapters.iter().filter(|c| !(hide_read && c.read)).collect();
        if !desc {
            list.reverse();
        }
        let rows: Vec<ChapterRow> = list
            .iter()
            .map(|c| {
                let (dl_state, progress) = a.manga.dl.get(&c.id).copied().unwrap_or((0, 0.0));
                let mut sub = fmt_date(c.published);
                if !c.scanlator.is_empty() {
                    if !sub.is_empty() {
                        sub.push_str(" · ");
                    }
                    sub.push_str(&c.scanlator);
                }
                let in_progress = !c.read && c.last_page > 0;
                if in_progress && c.page_count > 0 {
                    sub.push_str(&format!(" · page {}/{}", c.last_page + 1, c.page_count));
                }
                ChapterRow {
                    id: c.id as i32,
                    title: c.display_title().into(),
                    sub: sub.into(),
                    read: c.read,
                    in_progress,
                    downloaded: c.downloaded,
                    dl_state,
                    progress,
                    selected: a.manga.selected.contains(&c.id),
                }
            })
            .collect();
        (rows, a.manga.selected.len(), resume_target(&a.manga.chapters).map(|t| t.1))
    };
    if let Some(m) = set_model(st.get_chapters(), rows) {
        st.set_chapters(m);
    }
    st.set_chapter_selected_count(n_selected as i32);
    st.set_resume_label(label.unwrap_or_else(|| "Start reading".into()).into());
}

/// Which chapter "Resume" opens and how the button is labelled.
/// `chapters` is newest-first (ascending `position`).
pub fn resume_target(chapters: &[Chapter]) -> Option<(i64, String)> {
    if chapters.is_empty() {
        return None;
    }
    if let Some(c) = chapters.iter().filter(|c| !c.read && c.last_page > 0 && c.last_read_at > 0).max_by_key(|c| c.last_read_at) {
        return Some((c.id, format!("Resume · {}", short(c))));
    }
    let any_read = chapters.iter().any(|c| c.read);
    if let Some(c) = chapters.iter().rev().find(|c| !c.read) {
        let label = if any_read { format!("Continue · {}", short(c)) } else { "Start reading".to_string() };
        return Some((c.id, label));
    }
    chapters.last().map(|c| (c.id, "Read again".to_string()))
}

fn short(c: &Chapter) -> String {
    match c.number {
        Some(n) => format!("Ch. {}", tankobon_core::util::fmt_number(n)),
        None => c.display_title().chars().take(24).collect(),
    }
}

fn resume(ctx: &C) {
    let target = resume_target(&ctx.app().manga.chapters);
    if let Some((id, _)) = target {
        crate::reader::open_chapter(ctx, id, None);
    }
}

fn mark_previous_read(ctx: &C) {
    let ids = {
        let a = ctx.app();
        let Some(top) = a.manga.chapters.iter().filter(|c| a.manga.selected.contains(&c.id)).map(|c| c.position).min() else { return };
        a.manga.chapters.iter().filter(|c| c.position >= top).map(|c| c.id).collect::<Vec<_>>()
    };
    let c2 = ctx.clone();
    ctx.rt.spawn_blocking(move || {
        let _ = c2.db.set_read(&ids, true);
        let c3 = c2.clone();
        c2.ui(move |_| reload(&c3));
    });
    ctx.app().manga.selected.clear();
}

fn toggle_library(ctx: &C) {
    let Some(w) = ctx.win() else { return };
    let st = w.global::<State>();
    let id = ctx.app().manga.id;
    let add = !st.get_m_in_library();
    st.set_m_in_library(add);
    let c = ctx.clone();
    ctx.rt.spawn_blocking(move || {
        let _ = c.db.set_in_library(&[id], add);
        c.toast(if add { "Added to library" } else { "Removed from library" });
        let c2 = c.clone();
        c.ui(move |_| crate::library::refresh(&c2));
    });
}

/// Fetch details + chapters from the source. Without `force`, only manga that
/// were never fetched (or are not in the library, or are local) are synced.
pub fn sync(ctx: &C, force: bool) {
    let id = ctx.app().manga.id;
    if id == 0 {
        return;
    }
    if let Some(w) = ctx.win() {
        let st = w.global::<State>();
        st.set_m_loading(true);
        st.set_m_error("".into());
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let c2 = c.clone();
        let info = tokio::task::spawn_blocking(move || {
            let m = c2.db.get_manga(id).ok().flatten()?;
            let n = c2.db.chapters(id).map(|v| v.len()).unwrap_or(0);
            Some((m, n))
        })
        .await
        .ok()
        .flatten();
        let Some((m, n_chapters)) = info else { return };
        let needs = force || n_chapters == 0 || !m.in_library || m.source == "local" || now() - m.updated_at > 6 * 3600;
        if !needs {
            c.ui(|w| w.global::<State>().set_m_loading(false));
            return;
        }
        let res = tankobon_core::updates::sync_manga(&c.db, &c.registry, &m.source, &m.key).await;
        let c3 = c.clone();
        match res {
            Ok((_, new_ids)) => {
                if m.in_library && !new_ids.is_empty() && c.settings().auto_download_new {
                    let _ = c.downloads.enqueue(&new_ids);
                }
                c.ui(move |w| {
                    w.global::<State>().set_m_loading(false);
                    if c3.app().manga.id == id {
                        // metadata changed: let the cover reload too
                        reload(&c3);
                    }
                });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                c.ui(move |w| {
                    let st = w.global::<State>();
                    st.set_m_loading(false);
                    st.set_m_error(msg.into());
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: i64, pos: i32, read: bool, last_page: i32, last_read_at: i64) -> Chapter {
        Chapter { id, position: pos, read, last_page, last_read_at, number: Some((10 - pos) as f64), ..Default::default() }
    }

    #[test]
    fn resume_prefers_in_progress_chapter() {
        let list = vec![ch(1, 0, false, 0, 0), ch(2, 1, false, 5, 100), ch(3, 2, true, 9, 50)];
        let (id, label) = resume_target(&list).unwrap();
        assert_eq!(id, 2);
        assert!(label.starts_with("Resume"));
    }

    #[test]
    fn resume_picks_oldest_unread_otherwise() {
        let list = vec![ch(1, 0, false, 0, 0), ch(2, 1, false, 0, 0), ch(3, 2, true, 9, 50)];
        let (id, label) = resume_target(&list).unwrap();
        assert_eq!(id, 2);
        assert!(label.starts_with("Continue"));
        let fresh = vec![ch(1, 0, false, 0, 0), ch(2, 1, false, 0, 0)];
        let (id, label) = resume_target(&fresh).unwrap();
        assert_eq!((id, label.as_str()), (2, "Start reading"));
    }

    #[test]
    fn resume_when_everything_is_read() {
        let list = vec![ch(1, 0, true, 9, 5), ch(2, 1, true, 9, 4)];
        assert_eq!(resume_target(&list).unwrap(), (2, "Read again".to_string()));
        assert!(resume_target(&[]).is_none());
    }
}
