//! Het vaste linkscript komt mee met de gepinde HopOS-app-runtime.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!("cargo:rustc-link-arg-bins=-Thopapp.ld");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
