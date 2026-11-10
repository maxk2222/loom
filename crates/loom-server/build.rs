// Embedded-UI dirs: RustEmbed requires the folder to exist at compile
// time. CI/check builds run without `npm run build`, so create the dirs
// if absent (empty embed → /ui serves 404; release images always ship
// the real build).
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    for d in ["../../frontend/dist", "../../frontend/dist-client"] {
        let p = std::path::Path::new(&manifest).join(d);
        if !p.exists() {
            std::fs::create_dir_all(&p).unwrap();
        }
    }
    println!("cargo:rerun-if-changed=../../frontend/dist");
    println!("cargo:rerun-if-changed=../../frontend/dist-client");
}
