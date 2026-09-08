use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Build the WASM client (`client/pkg`) so a plain `cargo build` produces a
/// deployable binary without a separate `wasm-pack build` stage.
///
/// The check is idempotent: wasm-pack only runs when `client/pkg` is missing
/// or older than the client sources. Set `KRUST_SKIP_WASM_BUILD=1` to bypass
/// (useful for offline/CI builds that pass a prebuilt pkg).
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let client_dir = manifest_dir.join("..").join("client");

    let pkg_js = client_dir.join("pkg").join("terminal_client.js");
    let pkg_wasm = client_dir.join("pkg").join("terminal_client_bg.wasm");

    for entry in walk_rs(&client_dir.join("src")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        client_dir.join("res").join("server.html").display()
    );

    if env::var("KRUST_SKIP_WASM_BUILD").is_ok() {
        println!("cargo:warning=krust: KRUST_SKIP_WASM_BUILD set, skipping wasm client build");
        return;
    }

    if !pkg_js.exists() || !pkg_wasm.exists() || is_stale(&pkg_js, &client_dir) {
        build_wasm_client(&client_dir);
    }
}

/// True when any file under `client/src/` or `client/res/` is newer than `pkg_js`.
fn is_stale(pkg_js: &Path, client_dir: &Path) -> bool {
    let pkg_time = file_mtime(pkg_js).unwrap_or(SystemTime::UNIX_EPOCH);
    let newest_src = newest_mtime(&client_dir.join("src")).unwrap_or(SystemTime::UNIX_EPOCH);
    let newest_res = newest_mtime(&client_dir.join("res")).unwrap_or(SystemTime::UNIX_EPOCH);
    newest_src > pkg_time || newest_res > pkg_time
}

fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    for entry in walk_rs(dir) {
        if let Some(t) = file_mtime(&entry) {
            newest = Some(newest.map_or(t, |acc: SystemTime| acc.max(t)));
        }
    }
    newest
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Recursively collect source files (`.rs`) under `dir`.
fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_rs(&path));
        } else if path.extension().map_or(false, |e| e == "rs" || e == "html") {
            out.push(path);
        }
    }
    out
}

fn build_wasm_client(client_dir: &Path) {
    println!("cargo:warning=krust: building wasm client (wasm-pack build --target web)");
    let status = Command::new("wasm-pack")
        .args(["build", "--target", "web"])
        .current_dir(client_dir)
        .status()
        .expect("failed to spawn wasm-pack (install with: cargo install wasm-pack)");
    if !status.success() {
        panic!("wasm-pack build failed (exit {})", status.code().unwrap_or(-1));
    }
}