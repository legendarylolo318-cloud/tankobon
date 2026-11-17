//! Explorer: browse and search sources, then open a manga page.

use crate::ctx::{set_model, to_image, C};
use slint::{ComponentHandle, Model};
use tankobon_core::source::MangaEntry;
use tankobon_core::Manga;
use tankobon_ui::{Actions, MangaCard, SourceItem, State};

pub fn init(ctx: &C, w: &tankobon_ui::AppWindow) {
    let items: Vec<SourceItem> = ctx.registry.all().iter().map(|s| SourceItem { id: s.id().into(), name: s.name().into() }).collect();
    w.global::<State>().set_sources(slint::ModelRc::new(slint::VecModel::from(items)));
}

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    {
        let c = ctx.clone();
        a.on_open_source(move |i| {
            let Some(src) = c.registry.all().get(i as usize).cloned() else { return };
            if let Some(w) = c.win() {
                let st = w.global::<State>();
                st.set_current_source(i);
                st.set_source_name(src.name().into());
                st.set_explorer_query("".into());
            }
            c.app().explorer.source_id = src.id().to_string();
            load(&c, 0, String::new());
        });
    }
    {
        let c = ctx.clone();
        a.on_explorer_load(move |mode, q| load(&c, mode, q.to_string()));
    }
    {
        let c = ctx.clone();
        a.on_explorer_more(move || more(&c));
    }
    {
        let c = ctx.clone();
        a.on_need_explorer_cover(move |i| need_cover(&c, i));
    }
    {
        let c = ctx.clone();
        a.on_open_explorer_item(move |i| open_item(&c, i));
    }
}

fn load(ctx: &C, mode: i32, query: String) {
    if mode == 2 && query.trim().is_empty() {
        return;
    }
    {
        let mut a = ctx.app();
        a.explorer.mode = mode;
        a.explorer.query = query;
        a.explorer.page = 0;
        a.explorer.entries.clear();
        a.explorer.gen += 1;
    }
    if let Some(w) = ctx.win() {
        let st = w.global::<State>();
        st.set_explorer_mode(mode);
        st.set_explorer_error("".into());
        if let Some(m) = set_model(st.get_explorer_items(), vec![]) {
            st.set_explorer_items(m);
        }
    }
    more(ctx);
}

fn more(ctx: &C) {
    let (src_id, mode, query, page, gen) = {
        let mut a = ctx.app();
        a.explorer.page += 1;
        (a.explorer.source_id.clone(), a.explorer.mode, a.explorer.query.clone(), a.explorer.page, a.explorer.gen)
    };
    let Some(src) = ctx.registry.get(&src_id) else { return };
    if let Some(w) = ctx.win() {
        let st = w.global::<State>();
        st.set_explorer_loading(true);
        st.set_explorer_has_more(false);
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let res = match mode {
            0 => src.latest(page).await,
            1 => src.popular(page).await,
            _ => src.search(&query, page).await,
        };
        if c.app().explorer.gen != gen {
            return;
        }
        match res {
            Ok(p) => {
                c.app().explorer.entries.extend(p.items);
                let entries = c.app().explorer.entries.clone();
                let c2 = c.clone();
                let sid = src_id.clone();
                let flags = tokio::task::spawn_blocking(move || {
                    entries.iter().map(|e| c2.db.find_manga(&sid, &e.key).ok().flatten().is_some_and(|m| m.in_library)).collect::<Vec<bool>>()
                })
                .await
                .unwrap_or_default();
                let entries = c.app().explorer.entries.clone();
                c.ui(move |w| {
                    let st = w.global::<State>();
                    let old = st.get_explorer_items();
                    let cards: Vec<MangaCard> = entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let prev = old.row_data(i).filter(|r| r.has_cover);
                            MangaCard {
                                id: i as i32,
                                title: e.title.clone().into(),
                                cover: prev.as_ref().map(|r| r.cover.clone()).unwrap_or_default(),
                                has_cover: prev.is_some(),
                                unread: 0,
                                new_count: 0,
                                selected: false,
                                in_library: flags.get(i).copied().unwrap_or(false),
                            }
                        })
                        .collect();
                    if let Some(m) = set_model(st.get_explorer_items(), cards) {
                        st.set_explorer_items(m);
                    }
                    st.set_explorer_has_more(p.has_next);
                    st.set_explorer_loading(false);
                    if entries.is_empty() {
                        st.set_explorer_error("Nothing found.".into());
                    }
                });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                c.ui(move |w| {
                    let st = w.global::<State>();
                    st.set_explorer_loading(false);
                    st.set_explorer_error(msg.into());
                });
            }
        }
    });
}

fn need_cover(ctx: &C, i: i32) {
    let (entry, src_id, gen) = {
        let a = ctx.app();
        (a.explorer.entries.get(i as usize).cloned(), a.explorer.source_id.clone(), a.explorer.gen)
    };
    let Some(entry) = entry else { return };
    if let Some(w) = ctx.win() {
        if w.global::<State>().get_explorer_items().row_data(i as usize).is_some_and(|r| r.has_cover) {
            return;
        }
    }
    let key = format!("exp:{gen}:{i}");
    if !ctx.app().covers_inflight.insert(key.clone()) {
        return;
    }
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        let buf = crate::covers::load(&c, &src_id, &entry.cover_url).await;
        c.app().covers_inflight.remove(&key);
        if let Some(buf) = buf {
            let c2 = c.clone();
            c.ui(move |w| {
                // A new search replaced the list: row `i` is a different manga now.
                if c2.app().explorer.gen != gen {
                    return;
                }
                let m = w.global::<State>().get_explorer_items();
                if let Some(mut r) = m.row_data(i as usize) {
                    r.cover = to_image(buf);
                    r.has_cover = true;
                    m.set_row_data(i as usize, r);
                }
            });
        }
    });
}

fn open_item(ctx: &C, i: i32) {
    let (entry, src_id) = {
        let a = ctx.app();
        (a.explorer.entries.get(i as usize).cloned(), a.explorer.source_id.clone())
    };
    let Some(MangaEntry { key, title, cover_url }) = entry else { return };
    let c = ctx.clone();
    ctx.rt.spawn_blocking(move || {
        match c.db.upsert_manga(&Manga { source: src_id, key, title, cover_url, ..Default::default() }) {
            Ok(id) => {
                let c2 = c.clone();
                c.ui(move |_| crate::manga::open(&c2, id));
            }
            Err(e) => c.toast(format!("Database error: {e}")),
        }
    });
}
