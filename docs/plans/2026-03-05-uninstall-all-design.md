# `--uninstall-all` Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `btrdasd setup --uninstall-all` that removes everything: installer-generated files AND cmake-installed binaries/configs/icons/man pages. Also enable `btrdasd-helper.service` in the cmake install step.

**Architecture:** Extend `SetupArgs` with a new `--uninstall-all` flag. The existing `uninstall()` function stays unchanged. A new `uninstall_all()` function calls `uninstall()` first, then stops the helper service and removes a hardcoded list of cmake-installed paths (binaries, D-Bus, polkit, systemd, man page, completions, desktop entry, icon, FFI lib/header). The install prefix comes from config.toml (defaulting to `/usr`). CMake's `install()` step gains a `systemctl enable btrdasd-helper.service` call.

**Tech Stack:** Rust 2024, clap 4.5, std::process::Command (systemctl), std::fs

---

### Task 1: Enable btrdasd-helper.service in CMake install

**Files:**
- Modify: `CMakeLists.txt:138-150` (BUILD_HELPER install block)

**Step 1: Add systemctl enable to CMake install**

In `CMakeLists.txt`, after the existing `install(FILES systemd/btrdasd-helper.service ...)` line inside the `if(BUILD_HELPER)` block, add:

```cmake
    install(CODE "
        execute_process(
            COMMAND systemctl daemon-reload
            ERROR_QUIET
        )
        execute_process(
            COMMAND systemctl enable btrdasd-helper.service
            ERROR_QUIET
        )
    ")
```

**Step 2: Verify**

Run: `cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build`
Expected: Build succeeds, no errors.

**Step 3: Commit**

```bash
git add CMakeLists.txt
git commit -m "fix: enable btrdasd-helper.service on cmake install"
```

---

### Task 2: Add `--uninstall-all` CLI flag

**Files:**
- Modify: `indexer/src/setup/mod.rs:11-27` (SetupArgs struct)
- Modify: `indexer/src/setup/mod.rs:29-60` (run function)

**Step 1: Add the flag to SetupArgs**

In `indexer/src/setup/mod.rs`, add to the `SetupArgs` struct after the `uninstall` field:

```rust
    /// Remove ALL files: generated configs, binaries, D-Bus, polkit, icons, man page, completions
    #[arg(long)]
    pub uninstall_all: bool,
```

**Step 2: Add dispatch in run()**

In `indexer/src/setup/mod.rs`, add an `else if args.uninstall_all` branch after the `args.uninstall` branch:

```rust
    } else if args.uninstall_all {
        let remove_db = dialoguer::Confirm::new()
            .with_prompt("Also remove the backup database?")
            .default(false)
            .interact()?;
        installer::uninstall_all(remove_db)?;
    }
```

**Step 3: Verify it compiles (will fail until Task 3)**

Run: `cd indexer && cargo check 2>&1 | head -5`
Expected: Error about `installer::uninstall_all` not existing — confirms the flag is wired up.

---

### Task 3: Implement `uninstall_all()`

**Files:**
- Modify: `indexer/src/setup/installer.rs` (add function after `uninstall()`)

**Step 1: Write the test**

Add to the `#[cfg(test)] mod tests` block in `installer.rs`:

```rust
    #[test]
    fn uninstall_all_removes_cmake_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Simulate cmake-installed files
        let bin_dir = base.join("usr/bin");
        let libexec_dir = base.join("usr/libexec");
        let lib_dir = base.join("usr/lib");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&libexec_dir).unwrap();
        std::fs::create_dir_all(&lib_dir).unwrap();

        let btrdasd = bin_dir.join("btrdasd");
        let gui = bin_dir.join("btrdasd-gui");
        let helper = libexec_dir.join("btrdasd-helper");
        let ffi = lib_dir.join("libbuttered_dasd_ffi.so");
        std::fs::write(&btrdasd, "bin").unwrap();
        std::fs::write(&gui, "bin").unwrap();
        std::fs::write(&helper, "bin").unwrap();
        std::fs::write(&ffi, "lib").unwrap();

        let paths = vec![
            btrdasd.to_string_lossy().to_string(),
            gui.to_string_lossy().to_string(),
            helper.to_string_lossy().to_string(),
            ffi.to_string_lossy().to_string(),
        ];

        let removed = remove_paths(&paths);
        assert_eq!(removed, 4);
        assert!(!btrdasd.exists());
        assert!(!gui.exists());
        assert!(!helper.exists());
        assert!(!ffi.exists());
    }
```

**Step 2: Run test to verify it fails**

Run: `cd indexer && cargo test --lib setup::installer::tests::uninstall_all_removes_cmake_files -- --nocapture 2>&1 | tail -5`
Expected: FAIL — `remove_paths` not found.

**Step 3: Implement `remove_paths()` helper**

Add above the `tests` module in `installer.rs`:

```rust
/// Remove a list of file paths, silently skipping any that don't exist.
/// Returns the count of files successfully removed.
fn remove_paths(paths: &[String]) -> usize {
    let mut removed = 0;
    for p in paths {
        let path = Path::new(p);
        if path.exists() && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}
```

**Step 4: Run test to verify it passes**

Run: `cd indexer && cargo test --lib setup::installer::tests::uninstall_all_removes_cmake_files -- --nocapture 2>&1 | tail -5`
Expected: PASS

**Step 5: Implement `cmake_installed_paths()`**

Add after `remove_paths()`:

```rust
/// Return the list of all files installed by `cmake --install`.
/// The `prefix` is the install prefix (e.g., `/usr` or `/usr/local`).
fn cmake_installed_paths(prefix: &str) -> Vec<String> {
    let p = |suffix: &str| format!("{prefix}/{suffix}");
    vec![
        // Binaries
        p("bin/btrdasd"),
        p("bin/btrdasd-gui"),
        p("libexec/btrdasd-helper"),
        // FFI
        p("lib/libbuttered_dasd_ffi.so"),
        p("include/btrdasd_ffi.h"),
        // D-Bus
        p("share/dbus-1/system.d/org.dasbackup.Helper1.conf"),
        p("share/dbus-1/system-services/org.dasbackup.Helper1.service"),
        // Polkit
        p("share/polkit-1/actions/org.dasbackup.policy"),
        // Man page
        p("share/man/man1/btrdasd.1"),
        // Shell completions
        p("share/bash-completion/completions/btrdasd"),
        p("share/zsh/site-functions/_btrdasd"),
        p("share/fish/vendor_completions.d/btrdasd.fish"),
        // Desktop entry and icon
        p("share/applications/org.theboscoclub.btrdasd-gui.desktop"),
        p("share/icons/hicolor/scalable/apps/btrdasd-gui.svg"),
        // XML GUI
        p("share/kxmlgui5/btrdasd-gui/btrdasd-gui.rc"),
        // Backup scripts (cmake-installed, separate from setup-generated)
        p("lib/das-backup/backup-run.sh"),
        p("lib/das-backup/backup-verify.sh"),
        p("lib/das-backup/boot-archive-cleanup.sh"),
        p("lib/das-backup/das-partition-drives.sh"),
        p("lib/das-backup/install-backup-timer.sh"),
        p("lib/das-backup/config/btrbk.conf"),
        p("lib/das-backup/config/das-backup-email.conf.example"),
        // Systemd units (cmake-installed templates)
        "/lib/systemd/system/das-backup.service".to_string(),
        "/lib/systemd/system/das-backup-full.service".to_string(),
        "/lib/systemd/system/das-backup.timer".to_string(),
        "/lib/systemd/system/das-backup-full.timer".to_string(),
        "/lib/systemd/system/btrdasd-helper.service".to_string(),
    ]
}
```

**Step 6: Implement `uninstall_all()`**

Add as a public function after `uninstall()`:

```rust
/// Full uninstall: remove generated files (manifest), then cmake-installed files.
pub fn uninstall_all(remove_db: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: run the standard uninstall (manifest files, timers, config dir)
    uninstall(remove_db)?;

    // Phase 2: stop the helper service
    let _ = std::process::Command::new("systemctl")
        .args(["disable", "--now", "btrdasd-helper.service"])
        .status();

    // Phase 3: determine install prefix from config (default /usr)
    let prefix = Config::load(&PathBuf::from(CONFIG_FILE))
        .ok()
        .map(|c| c.general.install_prefix.clone())
        .unwrap_or_else(|| "/usr".to_string());

    let paths = cmake_installed_paths(&prefix);
    let removed = remove_paths(&paths);
    println!("Removed {} cmake-installed files.", removed);

    // Phase 4: clean up empty directories
    let _ = std::fs::remove_dir_all(format!("{prefix}/lib/das-backup"));
    let _ = std::fs::remove_dir("/var/lib/das-backup");

    let _ = std::process::Command::new("systemctl")
        .args(["daemon-reload"])
        .status();

    println!("Full uninstall complete.");
    Ok(())
}
```

**Step 7: Run all tests**

Run: `cd indexer && cargo test --lib 2>&1 | tail -5`
Expected: All tests pass (137+1 = 138).

**Step 8: Commit**

```bash
git add indexer/src/setup/mod.rs indexer/src/setup/installer.rs
git commit -m "feat: add --uninstall-all to remove all installed files"
```

---

### Task 4: Add --uninstall-all to help text and man page

**Files:**
- Modify: `docs/btrdasd.1` (man page, setup section)
- Modify: `docs/INSTALL.md` (uninstall section)

**Step 1: Update man page**

Add `--uninstall-all` to the setup subcommand options in `docs/btrdasd.1`.

**Step 2: Update INSTALL.md**

Add an "Uninstall All" section after the existing "Uninstall" section:

```markdown
### Full Uninstall (everything)

```bash
sudo btrdasd setup --uninstall-all
```

Removes all generated files (same as `--uninstall`), then also removes cmake-installed components: binaries (`btrdasd`, `btrdasd-gui`, `btrdasd-helper`), FFI library, D-Bus configs, polkit policy, systemd units, man page, shell completions, desktop entry, and icon. Prompts whether to remove the backup database.
```

**Step 3: Commit**

```bash
git add docs/btrdasd.1 docs/INSTALL.md
git commit -m "docs: add --uninstall-all to man page and install guide"
```

---

### Task 5: Final verification

**Step 1: Run full test suite**

Run: `cd indexer && cargo test --lib && cargo clippy --all-targets --all-features`
Expected: All tests pass, clippy clean.

**Step 2: Verify CLI help**

Run: `cargo run -- setup --help 2>&1`
Expected: Shows `--uninstall-all` with description.

**Step 3: Build and install**

Run: `cmake --build build && sudo cmake --install build`
Expected: Installs all components, enables `btrdasd-helper.service`.

**Step 4: Verify helper is enabled**

Run: `systemctl is-enabled btrdasd-helper.service`
Expected: `enabled`
