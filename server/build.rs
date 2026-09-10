use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build the WASM client (`target/wasm/`) so a plain `cargo build` produces a
/// deployable binary without a separate `wasm-pack build` stage.
///
/// The check is idempotent: wasm client only runs when `target/wasm/` is missing
/// or older than the client sources. Set `KRUST_SKIP_WASM_BUILD=1` to bypass
/// (useful for offline/CI builds that pass a prebuilt wasm).
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let client_dir = manifest_dir.join("..").join("client");

    let workspace_root = manifest_dir.parent().unwrap();
    let wasm_out = workspace_root
        .join("target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm");

    for entry in walk_rs(&client_dir.join("src")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    for res in ["server.html", "krust_runtime.js"] {
        println!(
            "cargo:rerun-if-changed={}",
            client_dir.join("res").join(res).display()
        );
    }
    println!("cargo:rerun-if-changed={}", wasm_out.display());

    if env::var("KRUST_SKIP_WASM_BUILD").is_ok() {
        println!("cargo:warning=krust: KRUST_SKIP_WASM_BUILD set, skipping wasm client build");
        return;
    }

    if !wasm_out.exists() || is_stale(&wasm_out, &client_dir) {
        if let Err(e) = build_raw_wasm(&manifest_dir, &client_dir) {
            eprintln!("cargo:warning=krust: wasm build failed: {}", e);
            eprintln!("cargo:warning=krust: re-run with KRUST_SKIP_WASM_BUILD=1");
        }
    }
}

/// Build the WASM client directly with cargo to wasm32-unknown-unknown
fn build_raw_wasm(manifest_dir: &Path, client_dir: &Path) -> Result<(), String> {
    // Use a dedicated target dir under target/wasm so this nested cargo
    // invocation does not deadlock on the locks the parent cargo holds.
    let workspace_root = manifest_dir.parent().unwrap();
    let wasm_target_dir = workspace_root.join("target").join("wasm");

    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .env("CARGO_TARGET_DIR", &wasm_target_dir)
        .current_dir(client_dir)
        .status()
        .map_err(|e| format!("failed to run cargo: {}", e))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed with exit code: {}",
            status.code().unwrap_or(-1)
        ));
    }

    Ok(())
}

/// Recursively collect files under `dir`.
fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_rs(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// True when any file under `client/src/` or `client/res/` is newer than `pkg_js`.
fn is_stale(pkg_js: &Path, client_dir: &Path) -> bool {
    let pkg_time = file_mtime(pkg_js).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let newest_src = newest_mtime(&client_dir.join("src")).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let newest_res = newest_mtime(&client_dir.join("res")).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    newest_src > pkg_time || newest_res > pkg_time
}

/// Get the newest modification time of files in `dir`.
fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    for entry in walk_rs(dir) {
        if let Some(t) = file_mtime(&entry) {
            newest = Some(newest.map_or(t, |acc: std::time::SystemTime| acc.max(t)));
        }
    }
    newest
}

/// Get file modification time.
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}