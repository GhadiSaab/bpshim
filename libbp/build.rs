use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=src/capi.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    cbindgen::generate(&crate_dir)
        .expect("cbindgen failed")
        .write_to_file("include/bp.h");
}
