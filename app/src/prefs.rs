//! Preferences: load/save settings, theme, cache, backup/restore.

use crate::ctx::C;
use slint::ComponentHandle;
use std::path::PathBuf;
use tankobon_core::imaging;
use tankobon_core::models::{ReaderMode, Scaling};
use tankobon_core::settings::Theme;
use tankobon_core::Settings;
use tankobon_ui::{Actions, State};

pub fn load_into_ui(ctx: &C, w: &tankobon_ui::AppWindow) {
    let s = ctx.settings();
    let st = w.global::<State>();
    st.set_theme(s.theme.index());
    st.set_pref_mode(s.reader.mode.index());
    st.set_pref_scaling(s.reader.scaling.index());
    st.set_pref_double(s.reader.double_page);
    st.set_pref_crop(s.reader.crop_borders);
    st.set_pref_bg(s.reader.background);
    st.set_pref_page_number(s.show_page_number);
    st.set_pref_update_startup(s.update_on_startup);
    st.set_pref_auto_download(s.auto_download_new);
    st.set_pref_download_dir(s.download_dir.clone().into());
    st.set_pref_max_downloads(s.max_concurrent_downloads as i32);
    st.set_pref_cache_mb(s.cache_limit_mb as i32);
    st.set_pref_local_dir(s.local_dir.clone().into());
    st.set_pref_md_lang(s.mangadex_language.clone().into());
    st.set_pref_md_nsfw(s.mangadex_nsfw);
    st.set_library_sort(s.library_sort);
    st.set_library_sort_desc(s.library_sort_desc);
    st.set_chapters_desc(s.chapter_sort_desc);
    st.set_hide_read(s.hide_read_chapters);
    let default_backup = dirs_home().join("tankobon-backup.tankobon");
    st.set_backup_path(default_backup.to_string_lossy().to_string().into());
    update_cache_size(ctx);
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Push settings into the running components.
pub fn apply(ctx: &C) {
    let s = ctx.settings();
    for src in ctx.registry.all() {
        src.set_option("language", &s.mangadex_language);
        src.set_option("nsfw", if s.mangadex_nsfw { "true" } else { "false" });
        src.set_option("local_dir", &effective_local_dir(ctx, &s).to_string_lossy());
    }
    ctx.downloads.set_limits(effective_download_dir(ctx, &s), s.max_concurrent_downloads as usize);
}

pub fn effective_download_dir(ctx: &C, s: &Settings) -> PathBuf {
    if s.download_dir.trim().is_empty() {
        ctx.paths.default_download_dir()
    } else {
        PathBuf::from(s.download_dir.trim())
    }
}

pub fn effective_local_dir(ctx: &C, s: &Settings) -> PathBuf {
    if s.local_dir.trim().is_empty() {
        ctx.paths.local_dir()
    } else {
        PathBuf::from(s.local_dir.trim())
    }
}

pub fn update_cache_size(ctx: &C) {
    let dir = ctx.paths.cache.clone();
    let limit = ctx.settings().cache_limit_mb;
    ctx.blocking(
        move |_| imaging::dir_size(&dir),
        move |w, size| {
            let mb = size as f64 / 1_048_576.0;
            w.global::<State>().set_cache_size_text(format!("Currently using {mb:.1} MB of {limit} MB").into());
        },
    );
}

fn read_from_ui(ctx: &C, w: &tankobon_ui::AppWindow) -> Settings {
    let st = w.global::<State>();
    let mut s = ctx.settings();
    s.theme = Theme::from_index(st.get_theme());
    s.reader.mode = ReaderMode::from_index(st.get_pref_mode());
    s.reader.scaling = Scaling::from_index(st.get_pref_scaling());
    s.reader.double_page = st.get_pref_double();
    s.reader.crop_borders = st.get_pref_crop();
    s.reader.background = st.get_pref_bg();
    s.show_page_number = st.get_pref_page_number();
    s.update_on_startup = st.get_pref_update_startup();
    s.auto_download_new = st.get_pref_auto_download();
    s.download_dir = st.get_pref_download_dir().trim().to_string();
    s.max_concurrent_downloads = st.get_pref_max_downloads().clamp(1, 8) as u32;
    s.cache_limit_mb = st.get_pref_cache_mb().max(16) as u64;
    s.local_dir = st.get_pref_local_dir().trim().to_string();
    let lang = st.get_pref_md_lang().trim().to_string();
    s.mangadex_language = if lang.is_empty() { "en".into() } else { lang };
    s.mangadex_nsfw = st.get_pref_md_nsfw();
    s
}

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    {
        let c = ctx.clone();
        a.on_prefs_changed(move || {
            let Some(w) = c.win() else { return };
            let s = read_from_ui(&c, &w);
            c.set_settings(s);
            apply(&c);
            crate::covers::trim(&c);
            update_cache_size(&c);
        });
    }
    {
        let c = ctx.clone();
        a.on_clear_cache(move || {
            c.page_cache.clear();
            c.cover_cache.clear();
            let dir = c.paths.cache.clone();
            c.blocking(
                move |_| imaging::clear_dir(&dir).map_err(|e| e.to_string()),
                |w, r| {
                    let st = w.global::<State>();
                    match r {
                        Ok(()) => st.set_prefs_status("Cache cleared".into()),
                        Err(e) => st.set_prefs_status(format!("Could not clear cache: {e}").into()),
                    }
                },
            );
            let c2 = c.clone();
            c.rt.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let c3 = c2.clone();
                c2.ui(move |_| update_cache_size(&c3));
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_backup_now(move || {
            let Some(w) = c.win() else { return };
            let path = PathBuf::from(w.global::<State>().get_backup_path().to_string());
            c.blocking(
                move |c| tankobon_core::backup::export(&c.db, &path).map(|_| path.display().to_string()).map_err(|e| format!("{e:#}")),
                |w, r| {
                    let msg = match r {
                        Ok(p) => format!("Backup written to {p}"),
                        Err(e) => format!("Backup failed: {e}"),
                    };
                    w.global::<State>().set_prefs_status(msg.into());
                },
            );
        });
    }
    {
        let c = ctx.clone();
        a.on_restore_now(move || {
            let Some(w) = c.win() else { return };
            let path = PathBuf::from(w.global::<State>().get_backup_path().to_string());
            let c2 = c.clone();
            c.blocking(
                move |c| tankobon_core::backup::restore(&c.db, &path).map_err(|e| format!("{e:#}")),
                move |w, r| {
                    let st = w.global::<State>();
                    match r {
                        Ok(stats) => {
                            // settings may have changed
                            if let Ok(mut s) = c2.settings.write() {
                                *s = c2.db.load_settings();
                            }
                            load_into_ui(&c2, w);
                            apply(&c2);
                            crate::library::refresh(&c2);
                            st.set_prefs_status(format!("Restored {} manga, {} chapters", stats.manga, stats.chapters).into());
                        }
                        Err(e) => st.set_prefs_status(format!("Restore failed: {e}").into()),
                    }
                },
            );
        });
    }
    {
        let c = ctx.clone();
        a.on_browse_backup(move |save| {
            let c2 = c.clone();
            c.rt.spawn_blocking(move || {
                if let Some(p) = pick_backup_file(save) {
                    c2.ui(move |w| w.global::<State>().set_backup_path(p.to_string_lossy().to_string().into()));
                } else if cfg!(target_os = "android") {
                    c2.toast("Type the path into the field");
                }
            });
        });
    }
    {
        let c = ctx.clone();
        a.on_browse_download_dir(move || pick_dir(&c, true));
    }
    {
        let c = ctx.clone();
        a.on_browse_local_dir(move || pick_dir(&c, false));
    }
}

#[cfg(not(target_os = "android"))]
fn pick_backup_file(save: bool) -> Option<PathBuf> {
    let d = rfd::FileDialog::new().add_filter("Tankobon backup", &["tankobon"]);
    if save {
        d.set_file_name("tankobon-backup.tankobon").save_file()
    } else {
        d.pick_file()
    }
}

#[cfg(not(target_os = "android"))]
fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().pick_folder()
}

// No native file dialogs on Android: paths are typed into the fields instead.
#[cfg(target_os = "android")]
fn pick_backup_file(_save: bool) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "android")]
fn pick_folder() -> Option<PathBuf> {
    None
}

fn pick_dir(ctx: &C, downloads: bool) {
    let c = ctx.clone();
    ctx.rt.spawn_blocking(move || {
        if let Some(p) = pick_folder() {
            let s = p.to_string_lossy().to_string();
            let c2 = c.clone();
            c.ui(move |w| {
                let st = w.global::<State>();
                if downloads {
                    st.set_pref_download_dir(s.into());
                } else {
                    st.set_pref_local_dir(s.into());
                }
                w.global::<Actions>().invoke_prefs_changed();
                let _ = &c2;
            });
        }
    });
}
