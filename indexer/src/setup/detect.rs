#![allow(dead_code)]

// System detection module — detect block devices, BTRFS subvolumes,
// init system, package manager, and dependency availability.
//
// Design: parsing functions are pure and testable. Detection functions
// call system commands and are NOT tested in unit tests.

use serde::Deserialize;
use std::process::Command;

// ---------------------------------------------------------------------------
// Block device detection
// ---------------------------------------------------------------------------

/// A detected block device from lsblk output.
#[derive(Debug, Clone)]
pub struct BlockDevice {
    pub name: String,
    pub size: String,
    pub fstype: Option<String>,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub tran: Option<String>,
}

impl BlockDevice {
    /// Returns true if the device transport is USB.
    pub fn is_usb(&self) -> bool {
        self.tran.as_deref() == Some("usb")
    }

    /// Returns true if this looks like an EFI System Partition candidate:
    /// vfat filesystem and smaller than 2 GB.
    pub fn is_esp_candidate(&self) -> bool {
        self.fstype.as_deref() == Some("vfat") && self.size_bytes() < 2 * 1024 * 1024 * 1024
    }

    /// Parse a human-readable size string like "512M", "22T", "2G" into bytes.
    fn size_bytes(&self) -> u64 {
        let s = self.size.trim();
        if s.is_empty() {
            return 0;
        }

        // Find where the numeric part ends and the suffix begins
        let (num_part, suffix) = match s.find(|c: char| c.is_ascii_alphabetic()) {
            Some(pos) => (&s[..pos], &s[pos..]),
            None => (s, ""),
        };

        let base: f64 = match num_part.parse() {
            Ok(v) => v,
            Err(_) => return 0,
        };

        let multiplier: u64 = match suffix.to_uppercase().as_str() {
            "B" | "" => 1,
            "K" => 1024,
            "M" => 1024 * 1024,
            "G" => 1024 * 1024 * 1024,
            "T" => 1024 * 1024 * 1024 * 1024,
            "P" => 1024 * 1024 * 1024 * 1024 * 1024,
            _ => 1,
        };

        (base * multiplier as f64) as u64
    }
}

/// Internal serde struct for lsblk JSON output (top-level).
#[derive(Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<LsblkDevice>,
}

/// Internal serde struct for a single lsblk device.
#[derive(Deserialize)]
struct LsblkDevice {
    name: String,
    size: Option<String>,
    fstype: Option<String>,
    serial: Option<String>,
    model: Option<String>,
    tran: Option<String>,
}

/// Parse lsblk JSON output into a Vec of BlockDevice.
pub fn parse_lsblk_output(json: &str) -> Result<Vec<BlockDevice>, serde_json::Error> {
    let output: LsblkOutput = serde_json::from_str(json)?;
    Ok(output
        .blockdevices
        .into_iter()
        .map(|d| BlockDevice {
            name: d.name,
            size: d.size.unwrap_or_default(),
            fstype: d.fstype,
            serial: d.serial,
            model: d.model,
            tran: d.tran,
        })
        .collect())
}

/// Run `lsblk` and return detected block devices.
pub fn detect_block_devices() -> Vec<BlockDevice> {
    let output = Command::new("lsblk")
        .args(["--json", "-o", "NAME,SIZE,FSTYPE,SERIAL,MODEL,TRAN"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let json = String::from_utf8_lossy(&out.stdout);
            parse_lsblk_output(&json).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// BTRFS subvolume detection
// ---------------------------------------------------------------------------

/// A detected BTRFS subvolume.
#[derive(Debug, Clone)]
pub struct SubvolumeInfo {
    pub id: u64,
    pub name: String,
    pub top_level: u64,
}

/// Parse the text output of `btrfs subvolume list /` into SubvolumeInfo entries.
///
/// Each line has the format:
///   ID <id> gen <gen> top level <top> path <name>
pub fn parse_subvolume_output(output: &str) -> Vec<SubvolumeInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Expect at least: ID <id> gen <gen> top level <top> path <name>
            // Indices:         0    1   2    3    4     5     6    7    8...
            if parts.len() >= 9 && parts[0] == "ID" && parts[7] == "path" {
                let id = parts[1].parse().ok()?;
                let top_level = parts[6].parse().ok()?;
                // The path may contain spaces, so join everything from index 8
                let name = parts[8..].join(" ");
                Some(SubvolumeInfo {
                    id,
                    name,
                    top_level,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Run `btrfs subvolume list /` and return detected subvolumes.
pub fn detect_subvolumes() -> Vec<SubvolumeInfo> {
    let output = Command::new("btrfs")
        .args(["subvolume", "list", "/"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            parse_subvolume_output(&text)
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Init system detection
// ---------------------------------------------------------------------------

/// Detected init system.
#[derive(Debug, Clone, PartialEq)]
pub enum InitSystemDetected {
    Systemd,
    Openrc,
    Sysvinit,
}

/// Determine the init system from boolean flags indicating binary/path presence.
/// Priority: systemd > openrc > sysvinit > fallback(systemd).
pub fn detect_init_from_binaries(
    has_systemctl: bool,
    has_initd: bool,
    has_rc_service: bool,
) -> InitSystemDetected {
    if has_systemctl {
        InitSystemDetected::Systemd
    } else if has_rc_service {
        InitSystemDetected::Openrc
    } else if has_initd {
        InitSystemDetected::Sysvinit
    } else {
        // Fallback: assume systemd (most common)
        InitSystemDetected::Systemd
    }
}

/// Detect the running init system by checking for known binaries/paths.
pub fn detect_init_system() -> InitSystemDetected {
    let has_systemctl = which("systemctl");
    let has_initd = std::path::Path::new("/etc/init.d").exists();
    let has_rc_service = which("rc-service");
    detect_init_from_binaries(has_systemctl, has_initd, has_rc_service)
}

// ---------------------------------------------------------------------------
// Package manager detection
// ---------------------------------------------------------------------------

/// Detected package manager.
#[derive(Debug, Clone, PartialEq)]
pub enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Zypper,
    Apk,
    Unknown,
}

impl PackageManager {
    /// Return the install command string for this package manager with the given packages.
    pub fn install_cmd(&self, packages: &[&str]) -> String {
        let pkgs = packages.join(" ");
        match self {
            PackageManager::Pacman => format!("pacman -S --noconfirm {pkgs}"),
            PackageManager::Apt => format!("apt-get install -y {pkgs}"),
            PackageManager::Dnf => format!("dnf install -y {pkgs}"),
            PackageManager::Zypper => format!("zypper install -y {pkgs}"),
            PackageManager::Apk => format!("apk add {pkgs}"),
            PackageManager::Unknown => format!("# install manually: {pkgs}"),
        }
    }
}

/// Determine the package manager from boolean flags indicating binary presence.
/// Priority: pacman > apt > dnf > zypper > apk > unknown.
pub fn detect_pkgmgr_from_binaries(
    pacman: bool,
    apt: bool,
    dnf: bool,
    zypper: bool,
    apk: bool,
) -> PackageManager {
    if pacman {
        PackageManager::Pacman
    } else if apt {
        PackageManager::Apt
    } else if dnf {
        PackageManager::Dnf
    } else if zypper {
        PackageManager::Zypper
    } else if apk {
        PackageManager::Apk
    } else {
        PackageManager::Unknown
    }
}

/// Detect the system's package manager by checking for known binaries.
pub fn detect_package_manager() -> PackageManager {
    detect_pkgmgr_from_binaries(
        which("pacman"),
        which("apt-get"),
        which("dnf"),
        which("zypper"),
        which("apk"),
    )
}

// ---------------------------------------------------------------------------
// Dependency checking
// ---------------------------------------------------------------------------

/// Status of a single dependency binary.
#[derive(Debug, Clone)]
pub struct DepStatus {
    pub name: String,
    pub required: bool,
    pub path: Option<String>,
}

/// Check whether required and optional dependencies are available.
///
/// Always checks: btrbk, btrfs, smartctl, lsblk, mbuffer.
/// Conditionally checks: mailx (if email_enabled), rsync (if esp_mirror).
pub fn check_dependencies(email_enabled: bool, esp_mirror: bool) -> Vec<DepStatus> {
    let mut deps = vec![
        ("btrbk", true),
        ("btrfs", true),
        ("smartctl", true),
        ("lsblk", true),
        ("mbuffer", false),
    ];

    if email_enabled {
        deps.push(("mailx", true));
    }
    if esp_mirror {
        deps.push(("rsync", true));
    }

    deps.into_iter()
        .map(|(name, required)| DepStatus {
            name: name.to_string(),
            required,
            path: which_path(name),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Aggregate detection
// ---------------------------------------------------------------------------

/// All detected system information aggregated in one struct.
#[derive(Debug)]
pub struct SystemInfo {
    pub devices: Vec<BlockDevice>,
    pub subvolumes: Vec<SubvolumeInfo>,
    pub init_system: InitSystemDetected,
    pub package_manager: PackageManager,
    pub deps: Vec<DepStatus>,
}

impl SystemInfo {
    /// Run all detection functions and aggregate the results.
    pub fn detect() -> Self {
        Self {
            devices: detect_block_devices(),
            subvolumes: detect_subvolumes(),
            init_system: detect_init_system(),
            package_manager: detect_package_manager(),
            deps: check_dependencies(false, false),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a binary is available on PATH.
pub fn which(binary: &str) -> bool {
    Command::new("which")
        .arg(binary)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `which` and return the trimmed path if found.
pub fn which_path(binary: &str) -> Option<String> {
    Command::new("which")
        .arg(binary)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests (TDD — written first, implementation follows)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsblk_json() {
        let json = r#"{
            "blockdevices": [
                {"name":"sda","size":"22T","fstype":null,"serial":"ZXA0LMAE","model":"TOSHIBA_HDWT","tran":"sata"},
                {"name":"sdb","size":"512M","fstype":"vfat","serial":null,"model":"ESP","tran":"usb"},
                {"name":"nvme0n1","size":"2T","fstype":"btrfs","serial":"S123","model":"Samsung 990","tran":null}
            ]
        }"#;

        let devices = parse_lsblk_output(json).expect("parse lsblk JSON");
        assert_eq!(devices.len(), 3);

        // First device: sata, not usb, not esp
        assert_eq!(devices[0].name, "sda");
        assert_eq!(devices[0].serial.as_deref(), Some("ZXA0LMAE"));
        assert_eq!(devices[0].tran.as_deref(), Some("sata"));
        assert!(!devices[0].is_usb());
        assert!(!devices[0].is_esp_candidate());

        // Second device: usb, vfat, 512M < 2GB so esp candidate
        assert_eq!(devices[1].name, "sdb");
        assert!(devices[1].is_usb());
        assert!(devices[1].is_esp_candidate());

        // Third device: no tran, btrfs, 2T > 2GB so not esp candidate
        assert_eq!(devices[2].name, "nvme0n1");
        assert!(!devices[2].is_usb());
        assert!(!devices[2].is_esp_candidate());
    }

    #[test]
    fn parse_subvolume_list() {
        let output = "\
ID 256 gen 1000 top level 5 path @\n\
ID 257 gen 999 top level 5 path @home\n\
ID 258 gen 998 top level 5 path @snapshots\n\
ID 300 gen 500 top level 256 path @.archive.20260101T000000\n";

        let subs = parse_subvolume_output(output);
        assert_eq!(subs.len(), 4);
        assert_eq!(subs[0].name, "@");
        assert_eq!(subs[0].id, 256);
        assert_eq!(subs[0].top_level, 5);
        assert_eq!(subs[1].name, "@home");
        assert_eq!(subs[1].id, 257);
        assert_eq!(subs[2].name, "@snapshots");
        assert_eq!(subs[3].name, "@.archive.20260101T000000");
        assert_eq!(subs[3].id, 300);
        assert_eq!(subs[3].top_level, 256);
    }

    #[test]
    fn detect_init_system_from_paths() {
        // systemd available → Systemd
        assert_eq!(
            detect_init_from_binaries(true, false, false),
            InitSystemDetected::Systemd
        );
        // Only /etc/init.d → Sysvinit
        assert_eq!(
            detect_init_from_binaries(false, true, false),
            InitSystemDetected::Sysvinit
        );
        // rc-service available → Openrc
        assert_eq!(
            detect_init_from_binaries(false, false, true),
            InitSystemDetected::Openrc
        );
        // systemd takes priority even if others present
        assert_eq!(
            detect_init_from_binaries(true, true, true),
            InitSystemDetected::Systemd
        );
        // Nothing detected → fallback to Systemd
        assert_eq!(
            detect_init_from_binaries(false, false, false),
            InitSystemDetected::Systemd
        );
    }

    #[test]
    fn detect_package_manager_from_binaries() {
        // Pacman available → Pacman
        assert_eq!(
            detect_pkgmgr_from_binaries(true, false, false, false, false),
            PackageManager::Pacman
        );
        // Apt available → Apt
        assert_eq!(
            detect_pkgmgr_from_binaries(false, true, false, false, false),
            PackageManager::Apt
        );
        // Dnf available → Dnf
        assert_eq!(
            detect_pkgmgr_from_binaries(false, false, true, false, false),
            PackageManager::Dnf
        );
        // Zypper available → Zypper
        assert_eq!(
            detect_pkgmgr_from_binaries(false, false, false, true, false),
            PackageManager::Zypper
        );
        // Apk available → Apk
        assert_eq!(
            detect_pkgmgr_from_binaries(false, false, false, false, true),
            PackageManager::Apk
        );
        // Nothing → Unknown
        assert_eq!(
            detect_pkgmgr_from_binaries(false, false, false, false, false),
            PackageManager::Unknown
        );
        // Pacman takes priority
        assert_eq!(
            detect_pkgmgr_from_binaries(true, true, true, true, true),
            PackageManager::Pacman
        );
    }
}
