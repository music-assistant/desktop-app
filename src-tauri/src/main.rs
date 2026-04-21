// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebKitGTK's DMABUF renderer fails on several common Linux setups
    // (notably NVIDIA drivers and some Wayland compositors), producing either
    // a blank white WebView or a Wayland `Error 71 (Protocol error)` crash
    // on launch. Disable it by default; users can opt back in by exporting
    // the variable themselves before launching the app.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    app_lib::run();
}
