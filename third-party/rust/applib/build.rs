//! Reikt het linker-script van een app-image aan.
//!
//! Een bibliotheek kan geen link-argumenten aan de binary geven die haar
//! gebruikt, wel een zoekpad: dat reist mee naar de uiteindelijke link. Dit
//! script zet `link.ld` onder de naam `hopapp.ld` in `OUT_DIR` en meldt die
//! map; de build.rs van de app (zie appspike) zegt daarna alleen
//! `-Thopapp.ld`. Zo staat het script op één plek, naast de code die zijn
//! symbolen leest.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_default());
    // Een build.rs is boot-code in de zin van het handboek (§6): falen is
    // hier een build die stopt, en de reden hoort erbij.
    #[expect(
        clippy::expect_used,
        reason = "een build die het script mist, moet luid stoppen"
    )]
    fs::copy("link.ld", out.join("hopapp.ld")).expect("copy applib/link.ld to OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
