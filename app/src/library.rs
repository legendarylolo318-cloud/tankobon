//! Library grid: listing, search/sort/categories, selection and bulk actions.

use crate::ctx::{set_model, to_image, C};
use slint::{ComponentHandle, Model};
use tankobon_core::db::LibraryQuery;
use tankobon_core::LibrarySort;
use tankobon_ui::{Actions, MangaCard, NamedItem, State};

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    {
        let c = ctx.clone();
        a.on_library_refresh(move || refresh(&c));
    }
    {
        let c = ctx.clone();
        a.on_need_cover(move |id| need_cover(&c, id));
    }
    {
        let c = ctx.clone();
        a.on_open_manga(move |id| crate::manga::open(&c, id as i64));
    }
    {
        let c = ctx.clone();
        a.on_card_clicked(move |id| {
            let selecting = c.win().map(|w| w.global::<State>().get_select_mode()).unwrap_or(false);
            if selecting {
                toggle(&c, id as i64);
            } else {
                crate::manga::open(&c, id as i64);
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_card_context(move |id| {
            if let Some(w) = c.win() {
                w.global::<State>().set_select_mode(true);
            }
            toggle(&c, id as i64);
        });
    }
    {
        let c = ctx.clone();
        a.on_select_all(move || {
            if let Some(w) = c.win() {
                let m = w.global::<State>().get_library();
                let ids: Vec<i64> = m.iter().map(|x| x.id as i64).collect();
                c.app().selected = ids.into_iter().collect();
                apply_selection(&c);
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_clear_selection(move || clear_selection(&c));
    }
    {
        let c = ctx.clone();
        a.on_selection_mark_read(move |read| {
            let ids = selected_ids(&c);
            c.blocking(move |c| c.db.set_manga_read(&ids, read), |_, _| {});
            clear_selection(&c);
            refresh_soon(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_selection_download(move || {
            let ids = selected_ids(&c);
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let chapters = c2.db.undownloaded_unread(&ids).unwrap_or_default();
                match c2.downloads.enqueue(&chapters) {
                    Ok(n) => c2.toast(format!("Queued {n} chapter(s) for download")),
                    Err(e) => c2.toast(format!("Download failed: {e}")),
                }
            });
            clear_selection(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_selection_remove(move |delete_files| {
            let ids = selected_ids(&c);
            let n = ids.len();
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                if delete_files {
                    let chapters = c2.db.downloaded_chapter_ids(&ids).unwrap_or_default();
                    let _ = c2.downloads.delete_downloaded(&chapters);
                }
                let _ = c2.db.set_in_library(&ids, false);
                let _ = c2.db.purge_orphans();
                c2.toast(format!("Removed {n} manga from library"));
                let c3 = c2.clone();
                c2.ui(move |_| refresh(&c3));
            });
            clear_selection(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_selection_move(move |cat| {
            let ids = selected_ids(&c);
            let cat = if cat < 0 { None } else { Some(cat as i64) };
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let _ = c2.db.move_to_category(&ids, cat);
                let c3 = c2.clone();
                c2.ui(move |_| refresh(&c3));
                c2.toast("Moved");
            });
            clear_selection(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_add_category(move |name| {
            let name = name.to_string();
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                match c2.db.add_category(&name) {
                    Ok(_) => {}
                    Err(e) => c2.toast(format!("Could not add category: {e}")),
                }
                let c3 = c2.clone();
                c2.ui(move |_| refresh(&c3));
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_delete_category(move |id| {
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                let _ = c2.db.delete_category(id as i64);
                let c3 = c2.clone();
                c2.ui(move |w| {
                    // fall back to "All" so the view is never left on a deleted category
                    w.global::<State>().set_category_index(0);
                    refresh(&c3);
                });
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_rename_category(move |id, name| {
            let name = name.to_string();
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                if let Err(e) = c2.db.rename_category(id as i64, &name) {
                    c2.toast(format!("Could not rename: {e}"));
                }
                let c3 = c2.clone();
                c2.ui(move |_| refresh(&c3));
            });
        });
    }
}

fn selected_ids(ctx: &C) -> Vec<i64> {
    ctx.app().selected.iter().copied().collect()
}

fn toggle(ctx: &C, id: i64) {
    {
        let mut a = ctx.app();
        if !a.selected.remove(&id) {
            a.selected.insert(id);
        }
    }
    apply_selection(ctx);
}

pub fn clear_selection(ctx: &C) {
    ctx.app().selected.clear();
    if let Some(w) = ctx.win() {
        w.global::<State>().set_select_mode(false);
    }
    apply_selection(ctx);
}

/// Mirror the selection set into the model (UI thread).
pub fn apply_selection(ctx: &C) {
    let Some(w) = ctx.win() else { return };
    let sel = ctx.app().selected.clone();
    let st = w.global::<State>();
    let m = st.get_library();
    for i in 0..m.row_count() {
        if let Some(mut c) = m.row_data(i) {
            let s = sel.contains(&(c.id as i64));
            if c.selected != s {
                c.selected = s;
                m.set_row_data(i, c);
            }
        }
    }
    st.set_selected_count(sel.len() as i32);
}

fn refresh_soon(ctx: &C) {
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let c2 = c.clone();
        c.ui(move |_| refresh(&c2));
    });
}

/// Reload the library from the database. Must be called on the UI thread.
pub fn refresh(ctx: &C) {
    let Some(w) = ctx.win() else { return };
    let st = w.global::<State>();
    let idx = st.get_category_index();
    let category = match idx {
        0 => None,
        1 => Some(0),
        n => st.get_categories().row_data((n - 2) as usize).map(|c| c.id as i64),
    };
    let q = LibraryQuery {
        search: st.get_library_search().to_string(),
        category,
        sort: LibrarySort::from_index(st.get_library_sort()),
        descending: st.get_library_sort_desc(),
    };
    // remember sort choices
    let mut s = ctx.settings();
    if s.library_sort != st.get_library_sort() || s.library_sort_desc != q.descending {
        s.library_sort = st.get_library_sort();
        s.library_sort_desc = q.descending;
        ctx.set_settings(s);
    }
    let gen = {
        let mut a = ctx.app();
        a.library_gen += 1;
        a.library_gen
    };
    st.set_library_loading(true);
    let c = ctx.clone();
    ctx.blocking(
        move |c| (c.db.library(&q).unwrap_or_default(), c.db.categories().unwrap_or_default()),
        move |w, (entries, cats)| {
            if c.app().library_gen != gen {
                return;
            }
            let st = w.global::<State>();
            let sel = c.app().selected.clone();
            // keep already loaded covers
            let old = st.get_library();
            let mut known = std::collections::HashMap::new();
            for r in old.iter() {
                if r.has_cover {
                    known.insert(r.id, r.cover.clone());
                }
            }
            let cards: Vec<MangaCard> = entries
                .iter()
                .map(|e| {
                    let id = e.manga.id as i32;
                    let (cover, has_cover) = match known.get(&id) {
                        Some(i) => (i.clone(), true),
                        None => (Default::default(), false),
                    };
                    MangaCard {
                        id,
                        title: e.manga.title.clone().into(),
                        cover,
                        has_cover,
                        unread: e.unread as i32,
                        new_count: e.new_count as i32,
                        selected: sel.contains(&e.manga.id),
                        in_library: true,
                    }
                })
                .collect();
            if let Some(m) = set_model(st.get_library(), cards) {
                st.set_library(m);
            }
            let items: Vec<NamedItem> = cats.iter().map(|c| NamedItem { id: c.id as i32, name: c.name.clone().into(), checked: false }).collect();
            if let Some(m) = set_model(st.get_categories(), items) {
                st.set_categories(m);
            }
            if st.get_category_index() >= 2 + cats.len() as i32 {
                st.set_category_index(0);
            }
            st.set_library_loading(false);
            apply_selection(&c);
        },
    );
}

fn need_cover(ctx: &C, id: i32) {
    let Some(w) = ctx.win() else { return };
    let m = w.global::<State>().get_library();
    if m.iter().any(|r| r.id == id && r.has_cover) {
        return;
    }
    let key = format!("lib:{id}");
    if !ctx.app().covers_inflight.insert(key.clone()) {
        return;
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let c2 = c.clone();
        let manga = tokio::task::spawn_blocking(move || c2.db.get_manga(id as i64).ok().flatten()).await.ok().flatten();
        let buf = match manga {
            Some(m) => crate::covers::load(&c, &m.source, &m.cover_url).await,
            None => None,
        };
        c.app().covers_inflight.remove(&key);
        if let Some(buf) = buf {
            c.ui(move |w| {
                let m = w.global::<State>().get_library();
                let img = to_image(buf);
                for i in 0..m.row_count() {
                    if let Some(mut r) = m.row_data(i) {
                        if r.id == id {
                            r.cover = img;
                            r.has_cover = true;
                            m.set_row_data(i, r);
                            break;
                        }
                    }
                }
            });
        }
    });
}
