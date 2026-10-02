//! Verpakt de frontend uit `ui/`; `ui/VERSION` is de ene bron van de assetversie.
use std::{
    fs, io,
    path::{Path, PathBuf},
};
fn files(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
fn main() -> io::Result<()> {
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| io::Error::other("manifest path missing"))?,
    );
    let ui = manifest.join("ui");
    let out = PathBuf::from(
        std::env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("output path missing"))?,
    );
    let source = fs::read_to_string(ui.join("VERSION"))?;
    let version = source.trim();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Err(io::Error::other("invalid frontend version"));
    }
    let html = fs::read_to_string(ui.join("ui.html"))?.replace("__SPIN_UI_VERSION__", version);
    fs::write(out.join("ui.html"), html)?;
    let assets = ui.join("assets");
    let mut paths = Vec::new();
    files(&assets, &mut paths)?;
    paths.sort();
    let mut generated = format!(
        "pub(crate) const VERSION: &str = {version:?};\npub(crate) static ASSETS: &[(&str, &[u8])] = &[\n"
    );
    for path in paths {
        let name = path.strip_prefix(&assets).map_err(io::Error::other)?;
        generated.push_str(&format!(
            "({:?}, include_bytes!({:?})),\n",
            name.to_string_lossy(),
            path.to_string_lossy()
        ));
    }
    generated.push_str("];\n");
    fs::write(out.join("assets.rs"), generated)?;
    println!("cargo:rerun-if-changed={}", ui.join("VERSION").display());
    println!("cargo:rerun-if-changed={}", ui.join("ui.html").display());
    println!("cargo:rerun-if-changed={}", assets.display());
    Ok(())
}
