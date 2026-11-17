//! Build-time setup per target:
//! * Windows: embeds the application icon and version info into the executable.
//! * Android: link fixes for the bundled libunrar (C++).

fn main() {
    println!("cargo:rerun-if-changed=../assets/tankobon.ico");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os == "android" {
        android_link_fixes();
    }

    #[cfg(windows)]
    if target_os == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/tankobon.ico");
        res.set("ProductName", "Tankobon");
        res.set("FileDescription", "Tankobon manga reader");
        if let Err(e) = res.compile() {
            panic!("embedding Windows resources failed: {e}");
        }
    }
}

/// `unrar_sys` asks for `-lpthread`, which Android does not ship (pthreads live
/// in bionic's libc), and links no C++ runtime at all. Provide an empty
/// `libpthread.a` and link the NDK's static libc++.
fn android_link_fixes() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // An empty `ar` archive is just its magic header.
    std::fs::write(out.join("libpthread.a"), b"!<arch>\n").expect("writing libpthread.a stub");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=c++_static");
    println!("cargo:rustc-link-lib=c++abi");
}
