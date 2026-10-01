//! `btrdasd setup` must refuse to start for anyone but root.
//!
//! This is the one property of `setup::run` an unprivileged test can observe
//! end to end, and it is observed through the real binary: the refusal is an
//! exit status and two lines on stderr, not a return value.

use std::process::Command;

fn is_root() -> bool {
    // SAFETY: geteuid() is always safe.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn setup_refuses_to_run_unprivileged() {
    if is_root() {
        // As root the command would really run against this host.
        eprintln!("SKIP setup_refuses_to_run_unprivileged: running as root");
        return;
    }

    // `--check` is the read-only mode: were the guard ever missing, this is
    // the invocation that changes nothing.
    let out = Command::new(env!("CARGO_BIN_EXE_btrdasd"))
        .args(["setup", "--check"])
        .output()
        .expect("spawn btrdasd");

    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: btrdasd setup requires root privileges.\nRun: sudo btrdasd setup\n"
    );
    assert!(
        out.stdout.is_empty(),
        "nothing may run before the refusal, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
