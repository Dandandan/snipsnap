use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SNAPPY_CPP_PREFIX");
    println!("cargo:rustc-check-cfg=cfg(snappy_cpp)");

    if let Ok(prefix) = std::env::var("SNAPPY_CPP_PREFIX") {
        let prefix = PathBuf::from(prefix);
        let include = prefix.join("include").join("snappy-c.h");
        let lib = prefix.join("lib");
        if include.exists() && lib.exists() {
            println!("cargo:rustc-cfg=snappy_cpp");
            println!("cargo:rustc-link-search=native={}", lib.display());
            println!("cargo:rustc-link-lib=dylib=snappy");
        } else {
            panic!(
                "SNAPPY_CPP_PREFIX must contain include/snappy-c.h and lib/: {}",
                prefix.display()
            );
        }
    }
}
