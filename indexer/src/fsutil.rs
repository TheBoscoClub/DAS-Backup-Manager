//! Small filesystem and process helpers shared by the modules that change
//! the backup configuration during a run.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output};

/// Replace `path` with `contents` in one step: write a sibling temp file,
/// flush it to disk, then rename it over the target. A reader sees the old
/// file or the new one, never a truncated one — which matters because
/// `config.toml` and `btrbk.conf` are rewritten while a backup run is using
/// them.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Best effort: the error being returned is the one that matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Every external command `adopt` and `expire` run goes through this, so
/// tests can script what `btrfs`, `findmnt` and `blkid` answer.
pub trait CommandRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output>;
}

/// The real thing.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output> {
        cmd.output()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_runner_captures_output_and_exit_status() {
        let out = SystemRunner
            .output(std::process::Command::new("sh").args(["-c", "printf hi; exit 3"]))
            .unwrap();
        assert_eq!(out.stdout, b"hi");
        assert_eq!(out.status.code(), Some(3));
        assert!(
            SystemRunner
                .output(&mut std::process::Command::new(
                    "/nonexistent/das-no-such-binary"
                ))
                .is_err()
        );
    }

    #[test]
    fn write_atomic_refuses_when_the_directory_is_missing_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/file.txt");
        assert!(write_atomic(&path, "x").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_atomic_keeps_the_old_file_when_the_temp_file_cannot_be_created() {
        // A directory where the temp file would go makes creation fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "old").unwrap();
        std::fs::create_dir(dir.path().join(".f.tmp")).unwrap();
        assert!(write_atomic(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }
}
