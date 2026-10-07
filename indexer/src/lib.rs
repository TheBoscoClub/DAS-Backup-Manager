pub mod adopt;
pub mod backup;
pub mod btrbk_conf;
pub mod caldate;
pub mod config;
pub mod db;
pub mod doctor;
pub mod expire;
pub mod forget;
pub mod fsutil;
pub mod health;
pub mod indexer;
pub mod maintenance;
pub mod mount;
pub mod progress;
pub mod reconcile;
pub mod recovery_os;
pub mod report;
pub mod restore;
pub mod scanner;
pub mod schedule;
pub mod scrub;
pub mod subvol;

/// The product version, all four segments, as `CMakeLists.txt` writes it
/// (`build.rs` reads it there).
pub const VERSION: &str = env!("BTRDASD_VERSION");

#[cfg(test)]
mod version_tests {
    use super::VERSION;

    fn repo_file(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn the_version_has_four_segments_and_matches_cmake() {
        assert_eq!(VERSION.split('.').count(), 4, "{VERSION}");
        let cmake = repo_file("CMakeLists.txt");
        assert!(
            cmake.contains(&format!("VERSION {VERSION}\n")),
            "CMakeLists.txt project VERSION is not {VERSION}"
        );
    }

    #[test]
    fn the_man_page_header_carries_the_same_version() {
        let man = repo_file("docs/btrdasd.1");
        let th = man
            .lines()
            .find(|l| l.starts_with(".TH "))
            .expect(".TH line");
        assert!(
            th.contains(&format!("\"{VERSION}\"")),
            "docs/btrdasd.1 {th:?} does not carry version {VERSION}"
        );
    }
}
