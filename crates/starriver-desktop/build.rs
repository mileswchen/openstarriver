extern crate napi_build;

use std::path::Path;

fn main() {
    napi_build::setup();

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let llama_base = format!("{}/../../mobile/llama.cpp/build", manifest_dir);
    let lib_llama = format!("{}/src/libllama.a", llama_base);

    if Path::new(&lib_llama).exists() {
        println!("cargo:rustc-cfg=has_llama_cpp");
    }
}
