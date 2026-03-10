use crate::types::CliName;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::process::Command;

/// Process-wide cache of discovered binary paths. Call [`clear_cache`] to reset.
///
/// NOTE: Discovery relies on unix-specific APIs (`which`, permission bits, NVM paths)
/// and is not fully functional on Windows.
static CACHE: Mutex<Option<HashMap<CliName, String>>> = Mutex::new(None);

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

async fn which(binary: &str) -> Option<String> {
    let output = Command::new("which").arg(binary).output().await.ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    None
}

fn find_nvm_binary(binary: &str) -> Option<String> {
    // Check $NVM_BIN
    if let Ok(nvm_bin) = std::env::var("NVM_BIN") {
        let p = PathBuf::from(&nvm_bin).join(binary);
        if is_executable(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    // Check ~/.nvm/versions/node/*/bin/ (newest first)
    let home = home_dir()?;
    let nvm_versions = home.join(".nvm/versions/node");
    if !nvm_versions.is_dir() {
        return None;
    }

    let mut versions: Vec<PathBuf> = std::fs::read_dir(&nvm_versions)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    // Sort descending by semver (newest first).
    // NVM dirs are named like "v20.11.0", "v18.17.1", etc.
    versions.sort_by(|a, b| {
        let parse_ver = |p: &Path| -> (u64, u64, u64) {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            let s = name.strip_prefix('v').unwrap_or(&name);
            let mut parts = s.split('.').map(|n| n.parse::<u64>().unwrap_or(0));
            (
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
            )
        };
        parse_ver(b).cmp(&parse_ver(a))
    });

    for dir in versions {
        let p = dir.join("bin").join(binary);
        if is_executable(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    None
}

const SEARCH_PATHS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

const HOME_RELATIVE_PATHS: &[&str] = &[".local/bin", ".bun/bin", ".npm-global/bin"];

const CLAUDE_EXTRA_PATHS: &[&str] = &[".claude/local/claude"];

async fn search_for_binary(cli: CliName) -> Option<String> {
    let binary = cli.to_string();

    // 1. which (PATH)
    if let Some(path) = which(&binary).await {
        return Some(path);
    }

    // 2. NVM paths (node-based CLIs)
    if let Some(path) = find_nvm_binary(&binary) {
        return Some(path);
    }

    // 3. Common install locations
    for dir in SEARCH_PATHS {
        let p = PathBuf::from(dir).join(&binary);
        if is_executable(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    // 4. Home-relative paths
    if let Some(home) = home_dir() {
        for rel in HOME_RELATIVE_PATHS {
            let p = home.join(rel).join(&binary);
            if is_executable(&p) {
                return Some(p.to_string_lossy().into_owned());
            }
        }

        // 5. CLI-specific paths
        if cli == CliName::Claude {
            for rel in CLAUDE_EXTRA_PATHS {
                let p = home.join(rel);
                if is_executable(&p) {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
    }

    None
}

/// Discover a specific CLI binary, caching the result.
pub async fn discover_binary(cli: CliName) -> Option<String> {
    // Check cache
    {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = guard.as_ref() {
            if let Some(path) = cache.get(&cli) {
                if is_executable(Path::new(path)) {
                    return Some(path.clone());
                }
            }
        }
    }

    let path = search_for_binary(cli).await?;

    // Cache result
    {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(HashMap::new);
        cache.insert(cli, path.clone());
    }

    Some(path)
}

/// Discover all available CLI binaries (concurrent).
pub async fn discover_all() -> Vec<(CliName, String)> {
    let (claude, codex, gemini) = tokio::join!(
        discover_binary(CliName::Claude),
        discover_binary(CliName::Codex),
        discover_binary(CliName::Gemini),
    );

    let mut results = Vec::new();
    if let Some(path) = claude {
        results.push((CliName::Claude, path));
    }
    if let Some(path) = codex {
        results.push((CliName::Codex, path));
    }
    if let Some(path) = gemini {
        results.push((CliName::Gemini, path));
    }
    results
}

/// Discover the first available CLI binary (preference: Claude > Codex > Gemini).
///
/// Runs all lookups concurrently and returns the highest-priority match.
pub async fn discover_first() -> Option<(CliName, String)> {
    let (claude, codex, gemini) = tokio::join!(
        discover_binary(CliName::Claude),
        discover_binary(CliName::Codex),
        discover_binary(CliName::Gemini),
    );

    if let Some(path) = claude {
        return Some((CliName::Claude, path));
    }
    if let Some(path) = codex {
        return Some((CliName::Codex, path));
    }
    if let Some(path) = gemini {
        return Some((CliName::Gemini, path));
    }
    None
}

/// Clear the binary discovery cache.
pub fn clear_cache() {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn nvm_version_sorting() {
        // Simulate the version sorting logic used in find_nvm_binary
        let parse_ver = |name: &str| -> (u64, u64, u64) {
            let s = name.strip_prefix('v').unwrap_or(name);
            let mut parts = s.split('.').map(|n| n.parse::<u64>().unwrap_or(0));
            (
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
            )
        };

        assert_eq!(parse_ver("v20.11.0"), (20, 11, 0));
        assert_eq!(parse_ver("v18.17.1"), (18, 17, 1));
        assert_eq!(parse_ver("v22.0.0"), (22, 0, 0));
        assert_eq!(parse_ver("invalid"), (0, 0, 0));
        assert_eq!(parse_ver("v1"), (1, 0, 0));

        // Sorting: newer versions should come first
        let mut versions = vec!["v18.17.1", "v22.0.0", "v20.11.0"];
        versions.sort_by(|a, b| parse_ver(b).cmp(&parse_ver(a)));
        assert_eq!(versions, vec!["v22.0.0", "v20.11.0", "v18.17.1"]);
    }

    #[test]
    fn is_executable_checks_permission_bits() {
        let dir = tempfile::tempdir().unwrap();

        // Non-executable file
        let non_exec = dir.path().join("not-exec");
        std::fs::write(&non_exec, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&non_exec));

        // Executable file
        let exec = dir.path().join("exec");
        std::fs::write(&exec, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&exec));

        // Non-existent path
        assert!(!is_executable(Path::new("/does/not/exist")));
    }

    #[test]
    fn clear_cache_resets_state() {
        // Populate cache
        {
            let mut guard = CACHE.lock().unwrap();
            let cache = guard.get_or_insert_with(HashMap::new);
            cache.insert(CliName::Claude, "/usr/bin/claude".into());
        }

        clear_cache();

        let guard = CACHE.lock().unwrap();
        assert!(guard.is_none());
    }

    #[test]
    fn cli_name_display() {
        assert_eq!(CliName::Claude.to_string(), "claude");
        assert_eq!(CliName::Codex.to_string(), "codex");
        assert_eq!(CliName::Gemini.to_string(), "gemini");
    }
}
