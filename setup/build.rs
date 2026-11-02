// Everything this exe does needs admin rights, so its manifest asks for them, and Windows
// shows the prompt however it's started.
fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        embed_manifest(
            new_manifest("MonitorSetup")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed Windows manifest");

        // Non-fatal: if no resource compiler is available, the build still succeeds.
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/icon.ico");
        // What Windows shows in the file's properties, and in the admin prompt when unsigned.
        // Code signing requires these to be set.
        res.set("ProductName", "Monitor");
        res.set("FileDescription", "Monitor setup");
        res.set("LegalCopyright", "Copyright (c) 2026 Oleksandr Rozdaibida");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed skipped: {e}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../assets/icon.ico");
}
