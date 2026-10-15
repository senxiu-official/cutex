fn main() {
    // Match the native Codex Windows stack reserve. The CLI's typed config and
    // command trees exceed the MSVC default during profile materialization.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bin=cutex=/STACK:8388608");
    }
}
