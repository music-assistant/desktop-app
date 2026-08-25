fn main() {
    println!("cargo:rerun-if-env-changed=MUSIC_ASSISTANT_DISTRIBUTION");
    // The main window navigates to the MA server's web UI, so every command it
    // invokes arrives from a remote origin. Since tauri 2.11.1, remote origins
    // can only reach app commands that are declared here and explicitly granted
    // to a capability with a `remote` block; undeclared commands are silently
    // denied. Keep this list in sync with the `generate_handler!` list.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "is_companion_app",
            "is_desktop_app",
            "is_linux",
            "is_macos",
            "get_app_version",
            "get_i18n_bundle",
            "server_connecting",
            "server_connect_failed",
            "check_server_reachable",
            "companion_ready",
            "navigate_to_launcher",
            "get_now_playing",
            "update_now_playing",
            "start_desktop_services",
            "start_discord_rpc",
            "start_rpc",
            "discover_servers",
            "get_settings",
            "set_setting",
            "set_string_setting",
            "set_int_setting",
            "list_audio_devices",
            "stop_sendspin",
            "restart_sendspin",
            "get_sendspin_status",
            "sendspin_command",
            "get_sendspin_player_id",
            "configure_sendspin",
        ]),
    ))
    .expect("failed to run tauri-build");
}
