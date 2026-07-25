use std::path::PathBuf;

fn main() {
    // Declare the cfg unconditionally so clippy never trips the
    // unexpected-cfg lint, whether or not we end up embedding the wasm.
    println!("cargo:rustc-check-cfg=cfg(embedded_wasm)");

    // The wasm build itself must not recurse into this logic.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        return;
    }
    println!("cargo:rerun-if-env-changed=ZJ_RADAR_WASM_PATH");

    // Embed a wasm that was built explicitly. Never start Cargo recursively
    // from a Cargo build script: on a clean target directory the child waits on
    // the package lock held by its parent. Without a prebuilt artifact the CLI
    // uses `run`'s existing first-use download fallback.
    if let Some(path) = locate_wasm() {
        println!("cargo:rerun-if-changed={}", path.display());
        println!("cargo:rustc-env=ZJ_RADAR_WASM_PATH={}", path.display());
        println!("cargo:rustc-cfg=embedded_wasm");
    }
}

fn locate_wasm() -> Option<PathBuf> {
    // 1. Explicit override (release/nix supply a prebuilt wasm).
    if let Ok(p) = std::env::var("ZJ_RADAR_WASM_PATH") {
        let path = PathBuf::from(&p);
        if path.is_file() {
            return Some(path);
        }
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| manifest.join("../../target"));
    let prebuilt = target.join("wasm32-wasip1/release/zj_radar.wasm");
    // 2. Prebuilt artifact (fast path for `just test` / dev).
    if prebuilt.is_file() {
        return Some(prebuilt);
    }
    None
}
