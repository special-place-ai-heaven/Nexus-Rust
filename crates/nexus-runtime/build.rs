//! Pins the proxy DLL's PE export names and ordinals.

use std::{env, path::PathBuf};

fn main() {
    let Some(manifest_directory) = env::var_os("CARGO_MANIFEST_DIR") else {
        panic!("Cargo did not provide CARGO_MANIFEST_DIR");
    };
    let definition = PathBuf::from(manifest_directory).join("proxy.def");

    println!("cargo::rerun-if-changed={}", definition.display());
    println!("cargo::rustc-cdylib-link-arg=/DEF:{}", definition.display());
}
