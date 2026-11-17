//! Embeds the application icon and version info into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=../assets/tankobon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/tankobon.ico");
        res.set("ProductName", "Tankobon");
        res.set("FileDescription", "Tankobon manga reader");
        if let Err(e) = res.compile() {
            panic!("embedding Windows resources failed: {e}");
        }
    }
}
