//! Takes the product version from `CMakeLists.txt` — the one place it is
//! written (`project(... VERSION a.b.c.d)`). Cargo's own version field cannot
//! hold four segments, so `btrdasd --version` would otherwise print three.
//! Falls back to the package version when the file is not there (a crate
//! built on its own).

fn cmake_version() -> Option<String> {
    let text = std::fs::read_to_string("../CMakeLists.txt").ok()?;
    // The `project(` call itself: the line that starts with it. A comment
    // that mentions `project(` must not count (one did, bd
    // DAS-Backup-Manager-laj: `split("project(").nth(1)` is the text between
    // the first two occurrences, which was then that comment's tail).
    let rest = text
        .split_inclusive('\n')
        .skip_while(|line| !line.starts_with("project("))
        .collect::<String>();
    let after = rest.split("VERSION").nth(1)?;
    let version: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!version.is_empty()).then_some(version)
}

fn main() {
    println!("cargo:rerun-if-changed=../CMakeLists.txt");
    let version = cmake_version()
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("cargo sets it"));
    println!("cargo:rustc-env=BTRDASD_VERSION={version}");
}
