use crate::types::CliName;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Process-wide cache of discovered binary paths. Call [`clear_cache`] to reset.
///
/// WORKS ON WINDOWS NOW, and the note that used to sit here saying otherwise was
/// accurate: this module shelled out to `which` (no such binary on Windows),
/// read `HOME` (Windows uses `USERPROFILE`), and searched `~/.nvm` and
/// `/opt/homebrew/bin`. Two crates replaced all three — `which` for PATH
/// lookup and `home` for the home directory — which is a net DELETION of
/// platform-specific code rather than an addition.
static CACHE: Mutex<Option<HashMap<CliName, String>>> = Mutex::new(None);

fn home_dir() -> Option<PathBuf> {
    home::home_dir()
}

/// Executable file extensions to try when probing a directory directly.
///
/// EMPTY ON UNIX — the binary is the bare name. On Windows an npm global
/// install writes THREE shims for one CLI: `claude` (a bash script, for
/// git-bash), `claude.cmd`, and `claude.ps1`. Only the `.cmd` is runnable by
/// `CreateProcess`, and the bare `claude` is a real file — so a naive
/// `path.is_file()` probe finds the bash script and hands back something
/// Windows cannot execute. That is worse than finding nothing, because the
/// failure surfaces at spawn time as a confusing error instead of as
/// "not installed".
#[cfg(windows)]
const EXE_EXTENSIONS: &[&str] = &["cmd", "exe", "bat"];
#[cfg(not(windows))]
const EXE_EXTENSIONS: &[&str] = &[];

/// The runnable file for `stem` inside `dir`, if there is one.
fn runnable_in(dir: &Path, stem: &str) -> Option<PathBuf> {
    if EXE_EXTENSIONS.is_empty() {
        let p = dir.join(stem);
        return is_executable(&p).then_some(p);
    }
    EXE_EXTENSIONS.iter().find_map(|ext| {
        let p = dir.join(format!("{stem}.{ext}"));
        is_executable(&p).then_some(p)
    })
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

/// Resolve `binary` on PATH.
///
/// The `which` CRATE, not a `which` PROCESS. Besides working on Windows at all,
/// it applies `PATHEXT` there, so `claude` resolves to `claude.cmd`. It is
/// synchronous and does no I/O beyond stat-ing PATH entries, so it does not
/// need `spawn_blocking` — and it removes a process spawn from a hot path that
/// used to fork a shell utility three times at startup (once per CLI).
fn which_on_path(binary: &str) -> Option<String> {
    which::which(binary)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
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
    versions.sort_by(|a, b| {
        let name_of = |p: &Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        parse_nvm_version(&name_of(b)).cmp(&parse_nvm_version(&name_of(a)))
    });

    for dir in versions {
        if let Some(p) = runnable_in(&dir.join("bin"), binary) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    None
}

/// `"v20.11.0"` → `(20, 11, 0)`. Unparseable components sort as 0.
///
/// A FREE FUNCTION SO THE TEST CAN CALL IT. It used to be a closure inside
/// `find_nvm_binary`, and the test for it re-implemented the same logic inline
/// — so `nvm_version_sorting` passed regardless of what the real sort did, and
/// would have kept passing if the production copy were deleted.
pub(crate) fn parse_nvm_version(name: &str) -> (u64, u64, u64) {
    let s = name.strip_prefix('v').unwrap_or(name);
    let mut parts = s.split('.').map(|n| n.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// WHY THERE ARE FALLBACKS AT ALL, given `which` searches PATH: a macOS GUI app
/// does not inherit the shell's PATH. An `npm -g` install lands somewhere the
/// Finder-launched process has never heard of, so PATH lookup alone reports a
/// CLI the user demonstrably has as missing. These are that gap, and they are
/// per-platform because the gap is.
#[cfg(unix)]
const SEARCH_PATHS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
/// Windows inherits the system PATH into GUI processes, so `which` covers the
/// normal case; this is for a per-user npm prefix that PATH may lag behind.
#[cfg(windows)]
const SEARCH_PATHS: &[&str] = &[];

#[cfg(unix)]
const HOME_RELATIVE_PATHS: &[&str] = &[".local/bin", ".bun/bin", ".npm-global/bin"];
/// `%APPDATA%` and nvm-windows both sit under the user profile, which is what
/// `home::home_dir()` returns here.
#[cfg(windows)]
const HOME_RELATIVE_PATHS: &[&str] = &["AppData/Roaming/npm", "AppData/Roaming/nvm", ".bun/bin"];

const CLAUDE_EXTRA_PATHS: &[&str] = &[".claude/local/claude"];

fn search_for_binary(cli: CliName) -> Option<String> {
    let binary = cli.to_string();

    // 1. PATH
    if let Some(path) = which_on_path(&binary) {
        return Some(path);
    }

    // 2. NVM paths (node-based CLIs)
    if let Some(path) = find_nvm_binary(&binary) {
        return Some(path);
    }

    // 3. Common install locations
    for dir in SEARCH_PATHS {
        if let Some(p) = runnable_in(Path::new(dir), &binary) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    // 4. Home-relative paths
    if let Some(home) = home_dir() {
        for rel in HOME_RELATIVE_PATHS {
            if let Some(p) = runnable_in(&home.join(rel), &binary) {
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

    let path = search_for_binary(cli)?;

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

    /// The REAL `parse_nvm_version`, not a copy of it.
    ///
    /// This test used to declare its own `parse_ver` closure and assert against
    /// that — so it passed no matter what `find_nvm_binary` actually did, and
    /// would have gone on passing if the production sort were deleted outright.
    /// It now calls the function the code calls.
    #[test]
    fn nvm_version_sorting() {
        assert_eq!(parse_nvm_version("v20.11.0"), (20, 11, 0));
        assert_eq!(parse_nvm_version("v18.17.1"), (18, 17, 1));
        assert_eq!(parse_nvm_version("v22.0.0"), (22, 0, 0));
        assert_eq!(parse_nvm_version("invalid"), (0, 0, 0));
        assert_eq!(parse_nvm_version("v1"), (1, 0, 0));

        let mut versions = vec!["v18.17.1", "v22.0.0", "v20.11.0"];
        versions.sort_by_key(|v| std::cmp::Reverse(parse_nvm_version(v)));
        assert_eq!(versions, vec!["v22.0.0", "v20.11.0", "v18.17.1"]);
    }

    /// THE WINDOWS BUG, PINNED. An `npm -g install` writes three shims for one
    /// CLI: `claude` (a bash script for git-bash), `claude.cmd`, and
    /// `claude.ps1`. Only the `.cmd` is runnable by `CreateProcess`.
    ///
    /// The old probe was `path.is_file()` on Windows, which is TRUE for the
    /// bash script — so discovery would succeed and return a path Windows
    /// cannot execute. A failure at spawn time, phrased as though the CLI were
    /// broken rather than as though we had picked the wrong file.
    ///
    /// One test, both platforms, opposite expectations — which is the point:
    /// each side asserts what "runnable" means where it runs.
    #[test]
    fn runnable_in_picks_a_file_the_platform_can_actually_execute() {
        let dir = tempfile::tempdir().unwrap();

        // The extensionless shim npm writes for git-bash. Present on both
        // platforms in this test so the Windows assertion is about CHOICE, not
        // about absence.
        let bare = dir.path().join("agentcli");
        std::fs::write(&bare, "#!/bin/sh\necho hi").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bare, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        #[cfg(windows)]
        {
            // Nothing runnable yet: the bare file exists but has no executable
            // extension, and that is exactly the case that used to pass.
            assert!(
                runnable_in(dir.path(), "agentcli").is_none(),
                "a bash shim with no extension is not runnable on Windows"
            );

            std::fs::write(dir.path().join("agentcli.cmd"), "@echo hi").unwrap();
            let found = runnable_in(dir.path(), "agentcli").expect("the .cmd shim");
            assert_eq!(found.extension().unwrap(), "cmd");
        }

        #[cfg(unix)]
        {
            let found = runnable_in(dir.path(), "agentcli").expect("the executable");
            assert_eq!(found, bare);

            // …and a file without the executable bit is not a find.
            let dir2 = tempfile::tempdir().unwrap();
            std::fs::write(dir2.path().join("agentcli"), "#!/bin/sh").unwrap();
            assert!(runnable_in(dir2.path(), "agentcli").is_none());
        }
    }

    /// PATH lookup goes through the `which` crate, so it exists on every
    /// platform. Uses the toolchain's own binary — present wherever these tests
    /// run, including the Windows CI job, where it must resolve `cargo.exe`.
    #[test]
    fn path_lookup_works_on_every_platform() {
        let found = which_on_path("cargo").expect("cargo is on PATH wherever cargo test runs");
        assert!(
            Path::new(&found).is_file(),
            "resolved to a real file: {found}"
        );
        assert!(which_on_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_checks_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        let non_exec = dir.path().join("not-exec");
        std::fs::write(&non_exec, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&non_exec));

        let exec = dir.path().join("exec");
        std::fs::write(&exec, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&exec));

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
