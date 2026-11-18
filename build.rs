//! Build script: a bigger main-thread stack on Windows.
//!
//! GPUI lays out and paints the element tree recursively on the main
//! thread, and debug builds use large stack frames. Windows gives the main
//! thread only 1 MB (Linux / macOS: 8 MB), so a deep enough view — the SQL
//! tab's results pane with its chart header — overflowed it in debug
//! builds ("thread 'main' has overflowed its stack"). Reserve 8 MB like
//! the other platforms; only address space is reserved, pages are
//! committed on use.

fn main() {
    const STACK: u32 = 8 * 1024 * 1024;
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" {
        if env == "msvc" {
            println!("cargo:rustc-link-arg-bins=/STACK:{STACK}");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,{STACK}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
