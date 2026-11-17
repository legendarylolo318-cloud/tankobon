//! Developer tool: drives the UI through a fixed script and saves screenshots to
//! `$TANKOBON_AUTOPILOT_OUT`. Build with `--features autopilot`.

use crate::ctx::C;
use slint::{ComponentHandle, Model};
use tankobon_ui::{Actions, AppWindow, State};

fn snap(w: &AppWindow, dir: &str, name: &str) {
    match w.window().take_snapshot() {
        Ok(buf) => {
            let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec());
            if let Some(img) = img {
                let _ = img.save(format!("{dir}/{name}.png"));
                eprintln!("[autopilot] saved {name}");
            }
        }
        Err(e) => eprintln!("[autopilot] snapshot failed: {e}"),
    }
}

/// Renderer benchmark: `TANKOBON_BENCH=webtoon|paged` opens the newest chapter of
/// the library manga `TANKOBON_BENCH_MANGA` and scrolls / turns pages for 20 s.
/// Combine with `SLINT_DEBUG_PERFORMANCE=refresh_full_speed,console` for FPS.
fn bench(w: &AppWindow, kind: String) {
    let weak = w.as_weak();
    let manga: i32 = std::env::var("TANKOBON_BENCH_MANGA").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    std::thread::spawn(move || {
        let ui = |f: Box<dyn FnOnce(&AppWindow) + Send>| {
            let _ = weak.upgrade_in_event_loop(move |w| f(&w));
        };
        std::thread::sleep(std::time::Duration::from_millis(1500));
        ui(Box::new(move |w| w.global::<Actions>().invoke_open_manga(manga)));
        std::thread::sleep(std::time::Duration::from_millis(2500));
        if kind == "dlall" {
            // crash-recovery test: queue everything and keep running until killed
            ui(Box::new(|w| w.global::<Actions>().invoke_download_all_unread()));
            eprintln!("[bench] queued all");
            return;
        }
        ui(Box::new(|w| {
            let st = w.global::<State>();
            let n = st.get_chapters().row_count();
            if let Some(r) = st.get_chapters().row_data(n.saturating_sub(1)) {
                w.global::<Actions>().invoke_chapter_open(r.id);
            }
        }));
        std::thread::sleep(std::time::Duration::from_millis(2500));
        if std::env::var("TANKOBON_BENCH_FULLSCREEN").is_ok() {
            ui(Box::new(|w| {
                w.global::<State>().set_r_fullscreen(true);
                let s = w.window().size();
                eprintln!("[bench] window {}x{}", s.width, s.height);
            }));
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        ui(Box::new(|w| {
            let s = w.window().size();
            eprintln!("[bench] window {}x{} scale {}", s.width, s.height, w.window().scale_factor());
        }));
        eprintln!("[bench] start {kind}");
        let start = std::time::Instant::now();
        let mut y = 0.0f32;
        while start.elapsed() < std::time::Duration::from_secs(20) {
            if kind == "webtoon" {
                y += 12.0;
                ui(Box::new(move |w| {
                    let st = w.global::<State>();
                    st.set_wt_goto(y);
                    st.set_wt_goto_seq(st.get_wt_goto_seq() + 1);
                }));
                std::thread::sleep(std::time::Duration::from_millis(16));
            } else {
                ui(Box::new(|w| w.global::<Actions>().invoke_reader_next()));
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        eprintln!("[bench] end");
        ui(Box::new(|_| {
            let _ = slint::quit_event_loop();
        }));
    });
}

pub fn start(ctx: &C, w: &AppWindow) {
    if let Ok(kind) = std::env::var("TANKOBON_BENCH") {
        bench(w, kind);
        return;
    }
    let Ok(dir) = std::env::var("TANKOBON_AUTOPILOT_OUT") else { return };
    let weak = w.as_weak();
    let _ = ctx;
    std::thread::spawn(move || {
        let step = |ms: u64, name: &'static str, f: Box<dyn FnOnce(&AppWindow) + Send>| {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let d = dir.clone();
            let _ = weak.upgrade_in_event_loop(move |w| {
                f(&w);
            });
            std::thread::sleep(std::time::Duration::from_millis(1800));
            let d2 = d.clone();
            let _ = weak.upgrade_in_event_loop(move |w| {
                if !name.is_empty() {
                    snap(&w, &d2, name);
                }
            });
        };
        // Full feature tour (network: MangaDex). Each step acts, waits, snapshots.
        let ch_ids = |w: &AppWindow| -> Vec<i32> { let m = w.global::<State>().get_chapters(); (0..m.row_count()).filter_map(|i| m.row_data(i)).map(|r| r.id).collect() };
        let query = std::env::var("TANKOBON_TOUR_QUERY").unwrap_or_else(|_| "yotsuba".into());
        let backup = format!("{dir}/backup.tankobon");
        step(1500, "01_library_empty", Box::new(|_| {}));
        step(200, "02_mangadex_popular", Box::new(|w| {
            w.global::<Actions>().invoke_show_view(1);
            w.global::<Actions>().invoke_open_source(0);
            w.global::<Actions>().invoke_explorer_load(1, "".into());
        }));
        std::thread::sleep(std::time::Duration::from_millis(3000));
        step(200, "03_mangadex_search", Box::new(move |w| {
            w.global::<Actions>().invoke_explorer_load(2, query.into());
        }));
        std::thread::sleep(std::time::Duration::from_millis(3000));
        step(200, "04_manga_page", Box::new(|w| w.global::<Actions>().invoke_open_explorer_item(0)));
        std::thread::sleep(std::time::Duration::from_millis(4000));
        step(200, "05_added_to_library", Box::new(|w| w.global::<Actions>().invoke_toggle_library()));
        step(200, "06_reader_rtl", Box::new(move |w| {
            if let Some(id) = ch_ids(w).last().copied() { w.global::<Actions>().invoke_chapter_open(id); }
        }));
        std::thread::sleep(std::time::Duration::from_millis(5000));
        step(200, "07_reader_next", Box::new(|w| { w.global::<Actions>().invoke_reader_next(); w.global::<Actions>().invoke_reader_next(); }));
        for (name, mode, scaling) in [("08_ltr_fitwidth", 1, 0), ("09_vertical_fitheight", 2, 1), ("10_vertical_original", 2, 2), ("11_webtoon", 3, 3)] {
            step(200, name, Box::new(move |w| {
                let st = w.global::<State>();
                st.set_r_mode(mode);
                st.set_r_scaling(scaling);
                w.global::<Actions>().invoke_reader_settings_changed();
            }));
        }
        step(200, "12_double_page_rtl", Box::new(|w| {
            let st = w.global::<State>();
            st.set_r_mode(0); st.set_r_scaling(3); st.set_r_double(true); st.set_r_crop(true);
            w.global::<Actions>().invoke_reader_settings_changed();
        }));
        step(200, "13_back_to_manga", Box::new(|w| {
            let st = w.global::<State>();
            st.set_r_double(false); st.set_r_crop(false);
            w.global::<Actions>().invoke_reader_settings_changed();
            w.global::<Actions>().invoke_reader_close();
        }));
        step(200, "14_downloads_queued", Box::new(move |w| {
            let ids = ch_ids(w);
            for id in ids.iter().rev().take(3) { w.global::<Actions>().invoke_chapter_toggle_select(*id); }
            w.global::<Actions>().invoke_chapters_download();
            w.global::<Actions>().invoke_show_view(4);
            w.global::<Actions>().invoke_close_manga();
            w.global::<Actions>().invoke_show_view(4);
        }));
        step(200, "15_download_paused", Box::new(|w| {
            let m = w.global::<State>().get_downloads();
            if let Some(r) = m.row_data(0) { w.global::<Actions>().invoke_download_pause(r.chapter_id); }
        }));
        step(200, "16_download_cancelled", Box::new(|w| {
            let m = w.global::<State>().get_downloads();
            if let Some(r) = m.row_data(m.row_count().saturating_sub(1)) { w.global::<Actions>().invoke_download_cancel(r.chapter_id); }
        }));
        step(200, "17_download_resumed", Box::new(|w| {
            let m = w.global::<State>().get_downloads();
            if let Some(r) = m.row_data(0) { w.global::<Actions>().invoke_download_resume(r.chapter_id); }
        }));
        std::thread::sleep(std::time::Duration::from_millis(15000));
        step(200, "18_downloads_after_wait", Box::new(|_| {}));
        step(200, "19_updates", Box::new(|w| { w.global::<Actions>().invoke_update_library(); w.global::<Actions>().invoke_show_view(2); }));
        std::thread::sleep(std::time::Duration::from_millis(4000));
        step(200, "20_history", Box::new(|w| w.global::<Actions>().invoke_show_view(3)));
        step(200, "21_category_added", Box::new(|w| {
            w.global::<Actions>().invoke_show_view(0);
            w.global::<Actions>().invoke_add_category("Favorites".into());
        }));
        step(200, "22_moved_to_category", Box::new(|w| {
            let st = w.global::<State>();
            w.global::<Actions>().invoke_select_all();
            if let Some(c) = st.get_categories().row_data(0) { w.global::<Actions>().invoke_selection_move(c.id); }
            st.set_category_index(2);
            w.global::<Actions>().invoke_library_refresh();
        }));
        step(200, "23_backup", Box::new(move |w| {
            w.global::<State>().set_backup_path(backup.into());
            w.global::<Actions>().invoke_show_view(5);
            w.global::<Actions>().invoke_backup_now();
        }));
        step(200, "24_restore", Box::new(|w| w.global::<Actions>().invoke_restore_now()));
        step(200, "25_dark_theme", Box::new(|w| { w.global::<State>().set_theme(1); w.global::<Actions>().invoke_prefs_changed(); }));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = weak.upgrade_in_event_loop(|_| {
            let _ = slint::quit_event_loop();
        });
    });
}
