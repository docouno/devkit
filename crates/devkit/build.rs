fn main() {
    println!("cargo:rerun-if-changed=src/darwin.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/darwin.c")
            .compile("devkit_darwin");
    }
}
