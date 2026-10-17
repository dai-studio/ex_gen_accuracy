// The SvelteKit build output (ui/build) is embedded into the binary. Make sure the
// folder exists so the crate compiles even before the UI has been built; the server
// then serves a minimal placeholder page instead of the SPA.
fn main() {
    let dir = std::path::Path::new("ui/build");
    if !dir.exists() {
        std::fs::create_dir_all(dir).expect("create ui/build");
    }
    println!("cargo:rerun-if-changed=ui/build");
}
