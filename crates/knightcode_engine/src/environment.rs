//! What the engine process receives, and where its binary is. Pure, so the
//! lifecycle's decisions are testable without a process.

use anyhow::{Result, anyhow};
use rand::RngCore as _;
use std::path::{Path, PathBuf};

pub const TOKEN_ENV: &str = "KNIGHTCODE_ENGINE_TOKEN";
pub const PORT_ENV: &str = "KNIGHTCODE_ENGINE_PORT";
pub const PATH_ENV: &str = "KNIGHTCODE_ENGINE_PATH";
pub const BINARY_NAME: &str = if cfg!(windows) {
    "knightcode-engine.exe"
} else {
    "knightcode-engine"
};

const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// `existing` with the loopback hosts present exactly once. A corporate
/// `HTTP_PROXY` otherwise swallows the IDE's own traffic to the engine.
pub fn loopback_no_proxy(existing: Option<&str>) -> String {
    let mut entries: Vec<String> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    for host in LOOPBACK {
        if !entries.iter().any(|entry| entry.eq_ignore_ascii_case(host)) {
            entries.push(host.to_owned());
        }
    }
    entries.join(",")
}

/// Fixes the IDE's own process environment. Runs in `main` before the HTTP
/// client is built, because that client reads `NO_PROXY` once, at
/// construction; every child process inherits the result.
pub fn ensure_loopback_no_proxy() {
    for key in ["NO_PROXY", "no_proxy"] {
        let value = loopback_no_proxy(std::env::var(key).ok().as_deref());
        // SAFETY: called from `main` before any other thread reads the environment.
        unsafe { std::env::set_var(key, value) };
    }
}

/// 24 random bytes as hex: 48 characters, above the engine's 32 minimum.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Variables set on top of the inherited environment, and variables removed
/// from it. `proxy` is what Zed's settings say (`agent_servers::load_proxy_env`).
pub fn engine_environment(
    token: &str,
    port: Option<u16>,
    proxy: &[(String, String)],
) -> (Vec<(String, String)>, Vec<&'static str>) {
    let no_proxy = loopback_no_proxy(std::env::var("NO_PROXY").ok().as_deref());
    let mut set = vec![
        (TOKEN_ENV.to_owned(), token.to_owned()),
        ("NO_PROXY".to_owned(), no_proxy.clone()),
        ("no_proxy".to_owned(), no_proxy),
    ];
    if let Some(port) = port {
        set.push((PORT_ENV.to_owned(), port.to_string()));
    }
    set.extend(proxy.iter().cloned());
    // DEBUG turns on library tracing in Node-style runtimes and floods stderr;
    // LD_PRELOAD is whatever the desktop session injected, which a Bun binary
    // does not want.
    let mut remove = vec!["DEBUG"];
    if cfg!(target_os = "linux") {
        remove.push("LD_PRELOAD");
    }
    (set, remove)
}

/// The setting, then the environment variable, then the binary beside the
/// IDE executable. The error names the path that was tried.
pub fn locate_binary(
    setting: Option<&Path>,
    env: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Result<PathBuf> {
    let candidate = setting
        .map(Path::to_path_buf)
        .or_else(|| env.map(Path::to_path_buf))
        .or_else(|| exe_dir.map(|dir| dir.join(BINARY_NAME)));
    let Some(candidate) = candidate else {
        return Err(anyhow!(
            "knightcode-engine was not found: set knightcode.engine_path or {PATH_ENV}, or place {BINARY_NAME} next to the IDE executable"
        ));
    };
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(anyhow!(
            "knightcode-engine was not found at {}: set knightcode.engine_path to the binary, or build one with `bun run build:engine`",
            candidate.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_added_once_and_case_insensitively() {
        assert_eq!(loopback_no_proxy(None), "127.0.0.1,localhost,::1");
        assert_eq!(
            loopback_no_proxy(Some("corp.example, LOCALHOST ,")),
            "corp.example,LOCALHOST,127.0.0.1,::1"
        );
    }

    #[test]
    fn the_engine_environment_carries_the_token_and_drops_debug() {
        let (set, remove) =
            engine_environment("t", Some(4321), &[("HTTP_PROXY".into(), "http://p".into())]);
        assert!(set.contains(&(TOKEN_ENV.to_string(), "t".to_string())));
        assert!(set.contains(&(PORT_ENV.to_string(), "4321".to_string())));
        assert!(set.contains(&("HTTP_PROXY".to_string(), "http://p".to_string())));
        assert!(
            set.iter()
                .any(|(key, value)| key == "NO_PROXY" && value.contains("127.0.0.1"))
        );
        assert!(remove.contains(&"DEBUG"));
        assert_eq!(remove.contains(&"LD_PRELOAD"), cfg!(target_os = "linux"));
        let (set, _) = engine_environment("t", None, &[]);
        assert!(!set.iter().any(|(key, _)| key == PORT_ENV));
    }

    #[test]
    fn a_token_is_48_hex_characters_and_fresh_each_time() {
        let token = generate_token();
        assert_eq!(token.len(), 48);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, generate_token());
    }

    #[test]
    fn the_binary_is_found_by_setting_then_env_then_sibling_and_absence_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join(BINARY_NAME);
        std::fs::write(&present, b"").unwrap();
        let missing = dir.path().join("nope").join(BINARY_NAME);

        assert_eq!(
            locate_binary(Some(&present), Some(&missing), None).unwrap(),
            present
        );
        assert_eq!(locate_binary(None, Some(&present), None).unwrap(), present);
        assert_eq!(
            locate_binary(None, None, Some(dir.path())).unwrap(),
            present
        );
        let error = locate_binary(Some(&missing), None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains("knightcode.engine_path"), "{error}");
        assert!(
            locate_binary(None, None, None)
                .unwrap_err()
                .to_string()
                .contains(PATH_ENV)
        );
    }
}
