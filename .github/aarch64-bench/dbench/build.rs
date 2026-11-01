fn main() {
    let lib = std::env::var("SNAPPY_LIB").expect("SNAPPY_LIB: directory of the C++ libsnappy");
    println!("cargo:rustc-link-search=native={lib}");
    println!("cargo:rustc-link-lib=dylib=snappy");
    println!("cargo:rerun-if-env-changed=SNAPPY_LIB");
}
