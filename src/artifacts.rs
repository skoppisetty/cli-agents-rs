use crate::error::{Error, Result};
use crate::types::RunOptions;

pub(crate) fn temp_dir(opts: &RunOptions, prefix: &str) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    match opts.artifact_dir.as_deref() {
        Some(root) => builder.tempdir_in(root).map_err(Error::Io),
        None => builder.tempdir().map_err(Error::Io),
    }
}

pub(crate) fn prompt_file(
    opts: &RunOptions,
    prefix: &str,
    suffix: &str,
) -> Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    builder.suffix(suffix);
    match opts.artifact_dir.as_deref() {
        Some(root) => builder.tempfile_in(root).map_err(Error::Io),
        None => builder.tempfile().map_err(Error::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_owned_root_is_an_error_not_an_ambient_fallback() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!(
            "cli-agents-missing-artifact-root-{}-{nonce}",
            std::process::id()
        ));
        let opts = RunOptions {
            artifact_dir: Some(missing.to_string_lossy().into_owned()),
            ..Default::default()
        };

        assert!(temp_dir(&opts, "cli-agents-test-").is_err());
        assert!(prompt_file(&opts, "cli-agents-test-", ".md").is_err());
    }
}
