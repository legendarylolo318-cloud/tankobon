//! Link fixes for the bundled libunrar (C++) on Android.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    // `unrar_sys` asks for `-lpthread`, which Android does not ship (pthreads live
    // in bionic's libc), and links no C++ runtime at all. Provide an empty
    // `libpthread.a` and link the NDK's static libc++.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // An empty `ar` archive is just its magic header.
    std::fs::write(out.join("libpthread.a"), b"!<arch>\n").expect("writing libpthread.a stub");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=c++_static");
    println!("cargo:rustc-link-lib=c++abi");
}
