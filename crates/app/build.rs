//! GitHub issue #447: on Windows only, embeds `resources/windows/app-icon.ico` and a version
//! resource (`ProductName`/`FileDescription` = "Jerry") into the built executable, via the
//! `winresource` build-dependency (`[target.'cfg(windows)'.build-dependencies]` in this crate's
//! own `Cargo.toml` - see that entry's comment for why `winresource` over upstream zed's own
//! hand-rolled-`.rc` mechanism). A genuine no-op everywhere else: the `target_os` check below runs
//! before `winresource` is ever referenced, and that crate isn't even a dependency of this build
//! on a non-Windows target.

fn main() {
    println!("cargo:rerun-if-changed=resources/windows/app-icon.ico");

    // `CARGO_CFG_TARGET_OS` reflects the real build *target*, unlike `cfg!(windows)`/
    // `#[cfg(windows)]` inside a build script, which reflect the *host* running cargo - the two
    // only coincide because this workspace never cross-compiles. Checking the env var here,
    // ahead of the `#[cfg(windows)]`-gated call below, matches the same belt-and-suspenders
    // pattern `vendor/zed/crates/gpui/build.rs` itself uses for its own manifest resource.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        #[cfg(windows)]
        embed_windows_resources();
    }
}

#[cfg(windows)]
fn embed_windows_resources() {
    use winresource::WindowsResource;

    // `WindowsResource::new()` already seeds `FileVersion`/`ProductVersion` (both the string
    // properties and the numeric FILEVERSION/PRODUCTVERSION block) from `CARGO_PKG_VERSION`,
    // which - since this crate's own `[package] version = { workspace = true }` - is the real
    // workspace version. Only `ProductName`/`FileDescription` need overriding: their own default
    // is `CARGO_PKG_NAME` ("app", this crate's internal binary name), not the product name this
    // app ships under.
    let mut resource = WindowsResource::new();
    resource
        .set_icon("resources/windows/app-icon.ico")
        .set("ProductName", "Jerry")
        .set("FileDescription", "Jerry");

    if let Err(err) = resource.compile() {
        println!("cargo:warning=failed to embed Windows icon/version resource: {err}");
        std::process::exit(1);
    }
}
