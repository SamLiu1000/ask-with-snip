fn main() {
    // tauri-build only watches tauri.conf.json, so an icon swap in icons/ would
    // otherwise be ignored and the previously cached Windows resource reused.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
