// Embed a Windows application manifest. The app runs as a normal user
// (asInvoker) — no forced UAC. Only CPU die temperature / clock / power need
// the PawnIO kernel driver (and thus admin); everything else works unelevated.
// Run the exe as administrator to unlock those CPU readings.
fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        embed_manifest(
            new_manifest("Monitor").requested_execution_level(ExecutionLevel::AsInvoker),
        )
        .expect("failed to embed Windows manifest");

        // Embed the application icon into the exe (file/taskbar icon).
        // Non-fatal: if no resource compiler is available, build still succeeds.
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        // What Windows shows in the file's properties. Code signing requires these to be set.
        res.set("ProductName", "Monitor");
        res.set("FileDescription", "Monitor");
        res.set("LegalCopyright", "Copyright (c) 2026 Oleksandr Rozdaibida");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed skipped: {e}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MONITOR_UPDATE_URL");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
