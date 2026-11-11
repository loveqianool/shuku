use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(frontend_dist)");
    // If the frontend dist exists at build time, embed it; otherwise the
    // binary serves a placeholder page (see src/static_files.rs).
    // TODO(main agent): ensure `frontend/dist` is built before release builds.
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
    if dist.join("index.html").is_file() {
        println!("cargo:rustc-cfg=frontend_dist");
        println!("cargo:rerun-if-changed=../../frontend/dist");
    } else {
        println!("cargo:warning=frontend/dist not found, building with placeholder page");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
