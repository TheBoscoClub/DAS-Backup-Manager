#![allow(dead_code)]

// System detection module — detect block devices, BTRFS subvolumes,
// init system, package manager, and dependency availability.
//
// Design: parsing functions are pure and take the command's output as input.
// Detection functions are thin wrappers that spawn the command; the decision
// of whether its output counts as an answer lives in `successful_stdout`.

use buttered_dasd::config::TargetRole;
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

    /// Parse a human-readable size string like "512M", "22T", "2G" into bytes.
    ///
    /// lsblk's SIZE column counts in powers of 1024 whatever the suffix looks
    /// like (a "2 TB" drive reads `1.8T`), so `K` is 1024 bytes here, not 1000.
    ///
    /// A size that cannot be read — empty, not a number, negative, or carrying
    /// a unit this does not know — is `None`, never a number: a made-up byte
    /// count is indistinguishable from a measured one.
    fn size_bytes(&self) -> Option<u64> {
        let s = self.size.trim();

        // Find where the numeric part ends and the suffix begins
        let (num_part, suffix) = match s.find(|c: char| c.is_ascii_alphabetic()) {
            Some(pos) => (&s[..pos], &s[pos..]),
            None => (s, ""),
        };

        let base: f64 = num_part.parse().ok()?;
        if base < 0.0 {
            return None;
        }

        let multiplier: u64 = match suffix.to_uppercase().as_str() {
            "B" | "" => 1,
            "K" => 1024,
            "M" => 1024 * 1024,
            "G" => 1024 * 1024 * 1024,
            "T" => 1024 * 1024 * 1024 * 1024,
            "P" => 1024 * 1024 * 1024 * 1024 * 1024,
            _ => return None,
        };

        Some((base * multiplier as f64) as u64)
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
    successful_stdout(Command::new("lsblk").args([
        "--json",
        "-o",
        "NAME,SIZE,FSTYPE,SERIAL,MODEL,TRAN",
    ]))
    .map(|json| parse_lsblk_output(&json).unwrap_or_default())
    .unwrap_or_default()
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
    successful_stdout(Command::new("btrfs").args(["subvolume", "list", "/"]))
        .map(|text| parse_subvolume_output(&text))
        .unwrap_or_default()
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
/// Conditionally checks: mailx (if email_enabled).
pub fn check_dependencies(email_enabled: bool) -> Vec<DepStatus> {
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
    /// The BTRFS filesystem UUID on the partition a target of this role uses
    /// on the drive with this serial — recorded as a new target's
    /// `mount_uuid`. A lookup, not detection: the wizard asks it once per
    /// serial the operator types.
    pub fs_uuid_for: fn(&str, &TargetRole) -> Option<String>,
}

impl SystemInfo {
    /// Run all detection functions and aggregate the results.
    pub fn detect() -> Self {
        Self {
            devices: detect_block_devices(),
            subvolumes: detect_subvolumes(),
            init_system: detect_init_system(),
            package_manager: detect_package_manager(),
            deps: check_dependencies(false),
            fs_uuid_for: buttered_dasd::health::btrfs_uuid_for_serial,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The stdout of a command, only if it ran and exited successfully.
///
/// Whatever a failing command printed before it failed is not an answer, and
/// neither is the silence of a command that could not be started: both are
/// `None`, so the detectors above report nothing rather than something partial.
fn successful_stdout(cmd: &mut Command) -> Option<String> {
    match cmd.output() {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => None,
    }
}

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

        // First device: sata, not usb
        assert_eq!(devices[0].name, "sda");
        assert_eq!(devices[0].serial.as_deref(), Some("ZXA0LMAE"));
        assert_eq!(devices[0].tran.as_deref(), Some("sata"));
        assert!(!devices[0].is_usb());

        // Second device: usb
        assert_eq!(devices[1].name, "sdb");
        assert!(devices[1].is_usb());

        // Third device: no tran
        assert_eq!(devices[2].name, "nvme0n1");
        assert!(!devices[2].is_usb());
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

    fn sized(size: &str) -> BlockDevice {
        BlockDevice {
            name: "sdx".to_string(),
            size: size.to_string(),
            fstype: None,
            serial: None,
            model: None,
            tran: None,
        }
    }

    #[test]
    fn size_bytes_counts_every_lsblk_unit_in_powers_of_1024() {
        const K: u64 = 1024;
        let cases: &[(&str, u64)] = &[
            // No unit, or an explicit B, is already bytes.
            ("0", 0),
            ("512", 512),
            ("512B", 512),
            ("3K", 3 * K),
            ("512M", 512 * K * K),
            ("2G", 2 * K * K * K),
            ("22T", 22 * K * K * K * K),
            ("3P", 3 * K * K * K * K * K),
            // Spelled out, so a slip in the constants above cannot hide one
            // in the code: the 22 TB backup drives and a 2 GiB partition.
            ("22T", 24_189_255_811_072),
            ("2G", 2_147_483_648),
            ("3P", 3_377_699_720_527_872),
            // lsblk prints one decimal place: "55.5M", "931.5G", "1.8T".
            ("1.5K", 1536),
            ("55.5M", 58_195_968),
            ("931.5G", 1_000_190_509_056),
            ("0.5T", 549_755_813_888),
            // Fractions of a byte are dropped, not rounded up.
            ("1.8T", 1_979_120_929_996),
            // Case and surrounding whitespace carry no meaning.
            ("2g", 2_147_483_648),
            ("  3K\n", 3 * K),
            ("512b", 512),
        ];
        for &(size, bytes) in cases {
            assert_eq!(sized(size).size_bytes(), Some(bytes), "size {size:?}");
        }
    }

    #[test]
    fn size_bytes_refuses_to_invent_a_number_for_an_unreadable_size() {
        // lsblk reports a null size as "" (see `parse_lsblk_output`).
        for size in [
            "", "   ", "G", "abc", "1.2.3G", "1,5G", "-5G", "-1", "5X", "2GiB", "2 G", "1e3",
        ] {
            assert_eq!(sized(size).size_bytes(), None, "size {size:?}");
        }
    }

    #[test]
    fn parse_lsblk_keeps_every_field_and_maps_a_null_size_to_empty() {
        // Trimmed from real `lsblk --json -o NAME,SIZE,FSTYPE,SERIAL,MODEL,TRAN`
        // output; partitions arrive nested under "children" and are not devices.
        let json = r#"{
           "blockdevices": [
              {
                 "name": "loop2",
                 "size": "4K",
                 "fstype": "squashfs",
                 "serial": null,
                 "model": null,
                 "tran": null
              },{
                 "name": "sdk",
                 "size": "1.8T",
                 "fstype": null,
                 "serial": "ZK208Q77",
                 "model": "ST2000DM008-2UB102",
                 "tran": "usb",
                 "children": [
                    {
                       "name": "sdk1",
                       "size": "1.5G",
                       "fstype": "vfat",
                       "serial": null,
                       "model": null,
                       "tran": null
                    }
                 ]
              },{
                 "name": "sr0",
                 "size": null,
                 "fstype": null,
                 "serial": null,
                 "model": null,
                 "tran": "sata"
              }
           ]
        }"#;

        let devices = parse_lsblk_output(json).expect("parse lsblk JSON");
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["loop2", "sdk", "sr0"]);

        assert_eq!(devices[0].size, "4K");
        assert_eq!(devices[0].size_bytes(), Some(4096));
        assert_eq!(devices[0].fstype.as_deref(), Some("squashfs"));
        assert_eq!(devices[0].serial, None);
        assert!(!devices[0].is_usb());

        assert_eq!(devices[1].size, "1.8T");
        assert_eq!(devices[1].fstype, None);
        assert_eq!(devices[1].serial.as_deref(), Some("ZK208Q77"));
        assert_eq!(devices[1].model.as_deref(), Some("ST2000DM008-2UB102"));
        assert!(devices[1].is_usb());

        assert_eq!(devices[2].size, "");
        assert_eq!(devices[2].size_bytes(), None);
    }

    #[test]
    fn parse_lsblk_rejects_text_that_is_not_lsblk_json() {
        assert!(parse_lsblk_output("").is_err());
        assert!(parse_lsblk_output("lsblk: not a block device").is_err());
        assert!(parse_lsblk_output(r#"{"devices": []}"#).is_err());
        assert!(
            parse_lsblk_output(r#"{"blockdevices": []}"#)
                .expect("an empty device list is valid")
                .is_empty()
        );
    }

    #[test]
    fn parse_subvolume_skips_every_line_that_is_not_a_subvolume_row() {
        let output = "\
ID 256 gen 1000 top level 5 path @\n\
\n\
WARNING: this line has more than nine words in it path @bogus\n\
XX 257 gen 999 top level 5 path @wrong-first-keyword\n\
ID 258 gen 998 top level 5 name @wrong-eighth-keyword\n\
ID 259 gen 997 top level 5 path\n\
ID 260 gen 996 top level 5\n\
ID 261\n\
ID nan gen 995 top level 5 path @bad-id\n\
ID 262 gen 994 top level nan path @bad-top-level\n\
ID 263 gen 993 top level 256 path @home/bosco/My Documents\n";

        let subs = parse_subvolume_output(output);
        let rows: Vec<(u64, u64, &str)> = subs
            .iter()
            .map(|s| (s.id, s.top_level, s.name.as_str()))
            .collect();
        assert_eq!(
            rows,
            [(256, 5, "@"), (263, 256, "@home/bosco/My Documents")]
        );
        assert!(parse_subvolume_output("").is_empty());
    }

    #[test]
    fn successful_stdout_is_none_unless_the_command_ran_and_exited_zero() {
        assert_eq!(
            successful_stdout(Command::new("sh").args(["-c", "printf 'ID 5\\n'"])),
            Some("ID 5\n".to_string())
        );
        // Output printed before a failure is not an answer.
        assert_eq!(
            successful_stdout(Command::new("sh").args(["-c", "printf partial; exit 3"])),
            None
        );
        assert_eq!(
            successful_stdout(&mut Command::new("/nonexistent/das-no-such-binary")),
            None
        );
    }

    #[test]
    fn detect_block_devices_lists_the_disks_lsblk_reports() {
        // Asked a second way: one bare name per whole device, no JSON involved.
        let expected: Vec<String> = successful_stdout(Command::new("lsblk").args([
            "--nodeps",
            "--noheadings",
            "-o",
            "NAME",
        ]))
        .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();

        let found: Vec<String> = detect_block_devices().into_iter().map(|d| d.name).collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn install_cmd_is_the_non_interactive_form_for_each_package_manager() {
        // The wizard prints these for the operator to run as-is, so each must
        // be a complete command that will not stop to ask a question.
        let pkgs = ["btrbk", "smartmontools"];
        let cases = [
            (
                PackageManager::Pacman,
                "pacman -S --noconfirm btrbk smartmontools",
            ),
            (
                PackageManager::Apt,
                "apt-get install -y btrbk smartmontools",
            ),
            (PackageManager::Dnf, "dnf install -y btrbk smartmontools"),
            (
                PackageManager::Zypper,
                "zypper install -y btrbk smartmontools",
            ),
            (PackageManager::Apk, "apk add btrbk smartmontools"),
            // No known manager: a comment, so pasting it into a shell runs nothing.
            (
                PackageManager::Unknown,
                "# install manually: btrbk smartmontools",
            ),
        ];
        for (mgr, expected) in cases {
            assert_eq!(mgr.install_cmd(&pkgs), expected, "{mgr:?}");
        }
        assert_eq!(
            PackageManager::Pacman.install_cmd(&["mbuffer"]),
            "pacman -S --noconfirm mbuffer"
        );
    }

    // A name no package installs.
    const NO_SUCH_BINARY: &str = "das-no-such-binary-7f3a9c";

    #[test]
    fn which_finds_a_real_binary_and_not_a_missing_one() {
        assert!(which("sh"));
        assert!(!which(NO_SUCH_BINARY));
    }

    #[test]
    fn which_path_is_the_binary_location_or_none_never_a_made_up_path() {
        let sh = which_path("sh").expect("sh is on PATH");
        assert!(sh.ends_with("/sh"), "{sh:?}");
        assert!(std::path::Path::new(&sh).is_absolute(), "{sh:?}");
        assert!(std::path::Path::new(&sh).is_file(), "{sh:?}");

        assert_eq!(which_path(NO_SUCH_BINARY), None);
    }

    #[test]
    fn check_dependencies_lists_each_tool_with_its_requirement_and_real_path() {
        fn summary(deps: &[DepStatus]) -> Vec<(&str, bool)> {
            deps.iter().map(|d| (d.name.as_str(), d.required)).collect()
        }

        // mbuffer only smooths the send stream; a backup runs without it.
        let base = [
            ("btrbk", true),
            ("btrfs", true),
            ("smartctl", true),
            ("lsblk", true),
            ("mbuffer", false),
        ];
        let without_email = check_dependencies(false);
        assert_eq!(summary(&without_email), base);

        // mailx is needed exactly when reports are to be mailed.
        let with_email = check_dependencies(true);
        let mut expected = base.to_vec();
        expected.push(("mailx", true));
        assert_eq!(summary(&with_email), expected);

        // A dependency is reported present only where it really resolves.
        for dep in &with_email {
            assert_eq!(dep.path, which_path(&dep.name), "{}", dep.name);
        }
    }
}
