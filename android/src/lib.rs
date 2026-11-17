//! Android shared library for Tankobon. `cargo apk build -p tankobon-android`
//! packages it; the NativeActivity glue calls `android_main`.

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    tankobon_app::android_run(app);
}
