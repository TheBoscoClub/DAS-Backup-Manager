//! A recovery drive's own ESP, read while it is mounted read-only for the
//! reading (bd DAS-Backup-Manager-sp78.20): every file a systemd-boot loader
//! entry names must be on it, or that entry does not boot. On 2026-10-07
//! drive B's ESP lacked the microcode images every entry named, and drive
//! A's lacked the fallback image its Fallback entry named.
//!
//! The partition is named by the target's `esp_partuuid`, never found: see
//! [`crate::mount::read_recovery_esp`] for how it is mounted, and
//! `.claude/rules/esp-safety.md` for why nothing here discovers an ESP.

use std::path::Path;

/// The keys of a loader entry (Boot Loader Specification type #1) that name
/// a file on the ESP.
const FILE_KEYS: [&str; 4] = ["linux", "initrd", "efi", "devicetree"];

/// What a reading of an ESP found: how much it had to boot, and what would
/// stop that boot.
#[derive(Debug, Clone, PartialEq)]
pub struct Findings {
    /// `loader/entries/*.conf` read.
    pub entries: usize,
    /// `EFI/Linux/*.efi`, counted only when there is no entry.
    pub unified: usize,
    /// One line per file an entry names that the ESP does not hold, or the
    /// one line saying there is nothing to boot.
    pub problems: Vec<String>,
    /// Why the ESP is still mounted after it was read, when it could not be
    /// taken down (bd DAS-Backup-Manager-s5jv.2): a WARN, never a boot
    /// problem.
    pub left_mounted: Option<String>,
}

impl Findings {
    /// The report's `ESP` row: a reading that found nothing wrong says what
    /// it read, so it is never mistaken for one that never ran.
    pub fn summary(&self) -> String {
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let read = if self.entries > 0 {
            plural(self.entries, "loader entry", "loader entries")
        } else {
            format!(
                "no loader entry, {}",
                plural(
                    self.unified,
                    "unified kernel image",
                    "unified kernel images"
                )
            )
        };
        if self.problems.is_empty() {
            format!("checked: {read}, every file they name is there")
        } else {
            format!(
                "checked: {read}, {} — see BOOT PROBLEM",
                plural(self.problems.len(), "problem", "problems")
            )
        }
    }
}

/// [`read_loader`]'s problems alone.
#[cfg(test)]
fn loader_problems(esp: &Path) -> Result<Vec<String>, String> {
    read_loader(esp).map(|f| f.problems)
}

/// What would stop a boot from the ESP mounted at `esp`: each file a
/// `loader/entries/*.conf` entry names that the ESP does not hold, and an
/// ESP with no entry and no unified kernel image (`EFI/Linux/*.efi`) at all.
/// `Err` when the ESP could not be read.
pub fn read_loader(esp: &Path) -> Result<Findings, String> {
    let mut entries = names_in(esp, "loader/entries")?
        .into_iter()
        .filter(|n| n.to_ascii_lowercase().ends_with(".conf"))
        .collect::<Vec<_>>();
    entries.sort();
    if entries.is_empty() {
        let unified = names_in(esp, "EFI/Linux")?
            .iter()
            .filter(|n| n.to_ascii_lowercase().ends_with(".efi"))
            .count();
        let problems = if unified > 0 {
            Vec::new()
        } else {
            vec![
                "the ESP has no loader/entries/*.conf and no EFI/Linux/*.efi: systemd-boot has \
                 nothing to boot"
                    .to_string(),
            ]
        };
        return Ok(Findings {
            entries: 0,
            unified,
            problems,
            left_mounted: None,
        });
    }
    let mut problems = Vec::new();
    let count = entries.len();
    for name in entries {
        let rel = format!("loader/entries/{name}");
        let text = std::fs::read_to_string(esp.join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next().filter(|k| FILE_KEYS.contains(k)) else {
                continue;
            };
            // `initrd` may list several; the others name one file.
            let paths: Vec<&str> = if key == "initrd" {
                words.collect()
            } else {
                words.next().into_iter().collect()
            };
            for path in paths {
                if let Some(why) = missing(esp, path)? {
                    problems.push(format!(
                        "loader entry {name} names {key} {path}, which {why}: that entry does not boot"
                    ));
                }
            }
        }
    }
    Ok(Findings {
        entries: count,
        unified: 0,
        problems,
        left_mounted: None,
    })
}

/// The names in the ESP directory `rel`; none when it does not exist.
fn names_in(esp: &Path, rel: &str) -> Result<Vec<String>, String> {
    match std::fs::read_dir(esp.join(rel)) {
        Ok(dir) => dir
            .map(|e| {
                e.map(|e| e.file_name().to_string_lossy().into_owned())
                    .map_err(|e| format!("{rel}: {e}"))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{rel}: {e}")),
    }
}

/// Why the file an entry names at `path` (relative to the ESP's root, `/`
/// or `\` separated, as systemd-boot reads it) is not there to boot, or
/// `None` when it is. A path climbing out of the ESP is never looked up.
/// The lookup goes through the vfat mount, so it ignores case as the
/// firmware's own FAT driver does.
fn missing(esp: &Path, path: &str) -> Result<Option<&'static str>, String> {
    let parts: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return Ok(Some("leaves the ESP"));
    }
    match std::fs::symlink_metadata(esp.join(parts.join("/"))) {
        Ok(m) if m.is_file() => Ok(None),
        Ok(_) => Ok(Some("is not a file on this drive's ESP")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Some("this drive's ESP does not have"))
        }
        Err(e) => Err(format!("{path}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// An ESP as CachyOS's systemd-boot setup leaves it: two entries naming
    /// a kernel, microcode and an initramfs each.
    fn esp() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        for f in [
            "vmlinuz-linux-cachyos",
            "amd-ucode.img",
            "initramfs-linux-cachyos.img",
            "initramfs-linux-cachyos-fallback.img",
        ] {
            write(root, f, "x");
        }
        write(
            root,
            "loader/entries/linux-cachyos.conf",
            "# Created by: sdboot-manage\ntitle   Linux Cachyos\noptions root=UUID=5eed0008-0000-4000-8000-000000000008 rw\n\
             linux   /vmlinuz-linux-cachyos\ninitrd  /amd-ucode.img\ninitrd  /initramfs-linux-cachyos.img\n",
        );
        write(
            root,
            "loader/entries/linux-cachyos-fallback.conf",
            "title Linux Cachyos (Fallback)\nlinux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img\ninitrd /initramfs-linux-cachyos-fallback.img\n",
        );
        write(
            root,
            "loader/loader.conf",
            "default linux-cachyos.conf\ntimeout 5\n",
        );
        d
    }

    #[test]
    fn an_esp_holding_every_file_its_entries_name_has_no_problem() {
        let d = esp();
        assert_eq!(loader_problems(d.path()), Ok(Vec::new()));
    }

    #[test]
    fn missing_microcode_is_a_problem_for_every_entry_that_names_it() {
        // Drive B, 2026-10-07.
        let d = esp();
        std::fs::remove_file(d.path().join("amd-ucode.img")).unwrap();
        let problems = loader_problems(d.path()).unwrap();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().all(|p| p.contains("initrd /amd-ucode.img")),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("linux-cachyos-fallback.conf"),
            "{problems:?}"
        );
        assert!(problems[1].contains("linux-cachyos.conf"), "{problems:?}");
    }

    #[test]
    fn a_missing_fallback_image_is_a_problem_for_the_fallback_entry_only() {
        // Drive A, 2026-10-07: the preset no longer built the fallback image.
        let d = esp();
        std::fs::remove_file(d.path().join("initramfs-linux-cachyos-fallback.img")).unwrap();
        let problems = loader_problems(d.path()).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("linux-cachyos-fallback.conf")
                && problems[0].contains("initrd /initramfs-linux-cachyos-fallback.img"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_missing_kernel_efi_or_devicetree_is_a_problem() {
        let d = esp();
        write(
            d.path(),
            "loader/entries/other.conf",
            "linux /EFI/gone/vmlinuz\ndevicetree /dtbs/gone.dtb\n",
        );
        write(
            d.path(),
            "loader/entries/shell.conf",
            "efi /EFI/tools/shellx64.efi\n",
        );
        let problems = loader_problems(d.path()).unwrap();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].contains("linux /EFI/gone/vmlinuz"),
            "{problems:?}"
        );
        assert!(
            problems[1].contains("devicetree /dtbs/gone.dtb"),
            "{problems:?}"
        );
        assert!(
            problems[2].contains("efi /EFI/tools/shellx64.efi"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_directory_where_a_file_is_named_is_a_problem() {
        let d = esp();
        std::fs::remove_file(d.path().join("amd-ucode.img")).unwrap();
        std::fs::create_dir(d.path().join("amd-ucode.img")).unwrap();
        assert_eq!(loader_problems(d.path()).unwrap().len(), 2);
    }

    #[test]
    fn a_path_that_leaves_the_esp_is_a_problem_and_is_never_looked_up() {
        let d = esp();
        // Present outside the ESP, so a lookup would find it.
        let outside = d.path().parent().unwrap().join("outside-the-esp.img");
        std::fs::write(&outside, "x").unwrap();
        write(
            d.path(),
            "loader/entries/zz.conf",
            "linux /vmlinuz-linux-cachyos\ninitrd /../outside-the-esp.img\n",
        );
        let problems = loader_problems(d.path()).unwrap();
        std::fs::remove_file(&outside).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("leaves the ESP"), "{problems:?}");
    }

    #[test]
    fn backslashes_and_a_missing_leading_slash_name_the_same_file() {
        let d = esp();
        write(
            d.path(),
            "loader/entries/zz.conf",
            "linux \\vmlinuz-linux-cachyos\ninitrd amd-ucode.img\n",
        );
        assert_eq!(loader_problems(d.path()), Ok(Vec::new()));
    }

    #[test]
    fn comments_titles_options_and_other_files_are_not_judged() {
        let d = esp();
        write(
            d.path(),
            "loader/entries/zz.conf",
            "# linux /commented-out\ntitle /not-a-path\noptions initrd=/not-a-key\n",
        );
        write(d.path(), "loader/entries/README", "linux /not-an-entry\n");
        assert_eq!(loader_problems(d.path()), Ok(Vec::new()));
    }

    #[test]
    fn an_esp_with_nothing_to_boot_is_a_problem_unless_it_holds_a_unified_image() {
        let d = tempfile::tempdir().unwrap();
        let problems = loader_problems(d.path()).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("nothing to boot"), "{problems:?}");
        std::fs::create_dir_all(d.path().join("loader/entries")).unwrap();
        assert_eq!(loader_problems(d.path()).unwrap().len(), 1);
        write(d.path(), "EFI/Linux/linux-cachyos.efi", "x");
        assert_eq!(loader_problems(d.path()), Ok(Vec::new()));
    }

    #[test]
    fn the_summary_says_what_was_read_whether_or_not_anything_is_missing() {
        let d = esp();
        let f = read_loader(d.path()).unwrap();
        assert_eq!(
            f.summary(),
            "checked: 2 loader entries, every file they name is there"
        );
        std::fs::remove_file(d.path().join("amd-ucode.img")).unwrap();
        assert_eq!(
            read_loader(d.path()).unwrap().summary(),
            "checked: 2 loader entries, 2 problems — see BOOT PROBLEM"
        );
        let u = tempfile::tempdir().unwrap();
        write(u.path(), "EFI/Linux/linux-cachyos.efi", "x");
        assert_eq!(
            read_loader(u.path()).unwrap().summary(),
            "checked: no loader entry, 1 unified kernel image, every file they name is there"
        );
    }

    #[test]
    fn an_unreadable_entry_is_an_error_not_a_pass() {
        let d = esp();
        std::fs::create_dir(d.path().join("loader/entries/dir.conf")).unwrap();
        let r = loader_problems(d.path());
        assert!(r.as_ref().is_err_and(|e| e.contains("dir.conf")), "{r:?}");
    }

    // --- bd DAS-Backup-Manager-s5jv.3 ------------------------------------

    #[test]
    fn every_image_one_initrd_line_lists_is_looked_up() {
        let d = esp();
        write(
            d.path(),
            "loader/entries/zz.conf",
            "linux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img /not-built.img\n",
        );
        let problems = loader_problems(d.path()).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("initrd /not-built.img"),
            "{problems:?}"
        );
    }

    #[test]
    fn an_entries_directory_that_cannot_be_listed_is_an_error_not_an_empty_esp() {
        let d = esp();
        std::fs::remove_dir_all(d.path().join("loader/entries")).unwrap();
        write(d.path(), "loader/entries", "not a directory");
        let r = loader_problems(d.path());
        assert!(
            r.as_ref().is_err_and(|e| e.contains("loader/entries")),
            "{r:?}"
        );
    }

    #[test]
    fn a_named_path_that_cannot_be_looked_up_is_an_error_not_a_missing_file() {
        // Through a file: ENOTDIR, not "the ESP does not have it".
        let d = esp();
        write(
            d.path(),
            "loader/entries/zz.conf",
            "linux /vmlinuz-linux-cachyos/inside-a-file\n",
        );
        let r = loader_problems(d.path());
        assert!(
            r.as_ref().is_err_and(|e| e.contains("inside-a-file")),
            "{r:?}"
        );
    }
}
