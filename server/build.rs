use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build the WASM client (`client/pkg`) so a plain `cargo build` produces a
/// deployable binary without a separate `wasm-pack build` stage.
///
/// The check is idempotent: wasm client only runs when `client/pkg` is missing
/// or older than the client sources. Set `KRUST_SKIP_WASM_BUILD=1` to bypass
/// (useful for offline/CI builds that pass a prebuilt pkg).
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let client_dir = manifest_dir.join("..").join("client");

    let pkg_wasm = client_dir.join("pkg").join("terminal_client_bg.wasm");

    for entry in walk_rs(&client_dir.join("src")) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    for res in ["server.html", "krust_runtime.js"] {
        println!(
            "cargo:rerun-if-changed={}",
            client_dir.join("res").join(res).display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        client_dir.join("pkg").join("terminal_client_bg.wasm").display()
    );

    if env::var("KRUST_SKIP_WASM_BUILD").is_ok() {
        println!("cargo:warning=krust: KRUST_SKIP_WASM_BUILD set, skipping wasm client build");
        return;
    }

    if !pkg_wasm.exists() || is_stale(&pkg_wasm, &client_dir) {
        if let Err(e) = build_raw_wasm(&client_dir) {
            eprintln!("cargo:warning=krust: wasm build failed: {}", e);
            eprintln!("cargo:warning=krust: re-run with KRUST_SKIP_WASM_BUILD=1");
        }
    }
}

/// Build the WASM client directly with cargo to wasm32-unknown-unknown
fn build_raw_wasm(client_dir: &Path) -> Result<(), String> {
    let pkg_dir = client_dir.join("pkg");
    std::fs::create_dir_all(&pkg_dir).map_err(|e| format!("failed to create pkg dir: {}", e))?;

    // Use a dedicated target dir (outside the workspace target/) so this nested
    // cargo invocation does not deadlock on the locks the parent cargo holds.
    let wasm_target_dir = client_dir.join(".wasm-target");

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

    let src_wasm = wasm_target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("terminal_client.wasm");
    let dst_wasm = pkg_dir.join("terminal_client_bg.wasm");
    std::fs::copy(src_wasm, dst_wasm).map_err(|e| format!("failed to copy wasm: {}", e))?;

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