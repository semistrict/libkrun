fn main() {
    #[cfg(target_os = "linux")]
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,-soname,libkrun.so.{}",
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap()
    );
    // fxdax's in-process kernel defines the Zircon syscalls as C functions, as
    // the vDSO would. Bind libkrun's calls to its own kernel rather than to
    // another copy in the process, such as a program's own fxdax.
    #[cfg(target_os = "linux")]
    if std::env::var_os("CARGO_FEATURE_FXDAX").is_some() {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-Bsymbolic-functions");
    }
    #[cfg(target_os = "macos")]
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,-install_name,libkrun.{}.dylib,-compatibility_version,{}.0.0,-current_version,{}.{}.0",
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_MINOR").unwrap()
    );
}
