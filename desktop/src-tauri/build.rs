fn main() {
    // Re-embed the icon resources whenever the generated icons change; the icon
    // binary is never referenced from Rust, so cargo has no other reason to
    // rebuild and the packaged executable would keep a stale icon.
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.icns");
    println!("cargo:rerun-if-changed=icons/32x32.png");
    println!("cargo:rerun-if-changed=icons/128x128.png");
    println!("cargo:rerun-if-changed=icons/128x128@2x.png");
    println!("cargo:rerun-if-changed=tauri.conf.json");

    tauri_build::build();
}