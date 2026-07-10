fn main() {
    println!("cargo:rerun-if-env-changed=MUSIC_ASSISTANT_DISTRIBUTION");
    tauri_build::build();
}
