#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Tankobon: a native manga reader.

fn main() -> anyhow::Result<()> {
    tankobon_app::desktop_main()
}
