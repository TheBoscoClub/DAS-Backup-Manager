use crate::progress::{LogLevel, ProgressCallback};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// Result of a restore operation.
#[derive(Debug)]
pub struct RestoreResult {
    pub files_restored: usize,
    pub bytes_restored: u64,
    pub errors: Vec<String>,
    pub duration_secs: u64,
}

/// An entry returned by snapshot browsing.
#[derive(Debug, Clone)]
pub struct BrowseEntry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

/// Browse files in a snapshot directory, returning entries matching an optional path prefix.
pub fn browse_snapshot(
    snapshot_path: &Path,
    prefix: Option<&str>,
) -> Result<Vec<BrowseEntry>, Box<dyn std::error::Error>> {
    let browse_dir = match prefix {
        Some(p) => snapshot_path.join(p),
        None => snapshot_path.to_path_buf(),
    };

    let mut entries: Vec<BrowseEntry> = Vec::new();

    for entry in std::fs::read_dir(&browse_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // DirEntry::metadata does not follow symlinks, so size/mtime are the link's own
        let metadata = entry.metadata()?;

        let size = if file_type.is_dir() {
            0
        } else {
            metadata.len()
        };

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let is_dir = file_type.is_dir();

        // Path relative to snapshot_path
        let abs_path = entry.path();
        let rel_path = abs_path
            .strip_prefix(snapshot_path)
            .unwrap_or(&abs_path)
            .to_string_lossy()
            .into_owned();

        let name = entry.file_name().to_string_lossy().into_owned();

        entries.push(BrowseEntry {
            path: rel_path,
            name,
            size,
            mtime,
            is_dir,
        });
    }

    entries.sort_by(browse_order);

    Ok(entries)
}

/// Listing order for a browsed directory: directories first, then
/// alphabetically by name (case-insensitive).
fn browse_order(a: &BrowseEntry, b: &BrowseEntry) -> std::cmp::Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    }
}

/// Resolve `path` as far as it exists on disk, then re-attach the part that
/// does not exist yet.
///
/// A restore destination is routinely a directory that has not been created,
/// so plain `canonicalize()` would fail on exactly the input we most need to
/// judge. Resolving the existing prefix is what makes a symlinked ancestor
/// impossible to hide behind.
fn resolve_for_policy(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let mut existing = path;
    let mut trailing = Vec::new();
    loop {
        if existing.exists() {
            let mut out = existing.canonicalize()?;
            for part in trailing.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                trailing.push(name.to_os_string());
                existing = parent;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no existing ancestor of {}", path.display()),
                ));
            }
        }
    }
}

/// Refuse a restore destination that policy does not permit.
///
/// Two checks, denylist first: [`RESTORE_DENIED_ROOTS`] can never be written to
/// even if someone lists one in `allowed_roots`, and the destination must then
/// fall under one of the configured roots. Both are applied to the RESOLVED
/// path, so a symlinked ancestor cannot smuggle a write past them; each root
/// is matched as written and resolved — an allowed root only through links
/// root owns ([`resolve_root`], bd DAS-Backup-Manager-sp78.18).
///
/// Before 0.7.20.0 there was no check at all: `restore_files` runs as root under
/// the D-Bus helper and would happily write into a systemd unit directory or a
/// package-manager hook directory (bd DAS-Backup-Manager-s05).
pub fn check_dest_allowed(
    dest: &Path,
    allowed_roots: &[String],
) -> Result<std::path::PathBuf, String> {
    let resolved = resolve_for_policy(dest)
        .map_err(|e| format!("Cannot resolve destination '{}': {e}", dest.display()))?;
    match judge_dest(resolved, allowed_roots) {
        DestPolicy::Allowed(p) => Ok(p),
        DestPolicy::Denied { resolved, root } => Err(format!(
            "Refusing to restore into '{}': '{root}' is never a permitted destination",
            resolved.display()
        )),
        DestPolicy::NotAllowed(resolved) => Err(format!(
            "Refusing to restore into '{}': not under any [restore] allowed_roots ({})",
            resolved.display(),
            allowed_roots.join(", ")
        )),
    }
}

/// What the destination policy says of a resolved path.
enum DestPolicy {
    Allowed(std::path::PathBuf),
    Denied {
        resolved: std::path::PathBuf,
        root: &'static str,
    },
    NotAllowed(std::path::PathBuf),
}

/// [`check_dest_allowed`]'s rule on a path already resolved.
fn judge_dest(resolved: std::path::PathBuf, allowed_roots: &[String]) -> DestPolicy {
    // Each root is compared as written and resolved: the destination is
    // resolved, and a root reached through a link (`/home -> var/home`)
    // otherwise never matched it (bd DAS-Backup-Manager-sp78.18). For the
    // denylist that only refuses more.
    let under = |root: &Path| resolved.starts_with(root);
    let under_resolved = |root: &Path| {
        under(root) || resolve_root(root, owner_of_link).is_some_and(|r| resolved.starts_with(r))
    };
    for denied in crate::config::RESTORE_DENIED_ROOTS {
        if under(Path::new(denied))
            || resolve_for_policy(Path::new(denied)).is_ok_and(|r| resolved.starts_with(r))
        {
            return DestPolicy::Denied {
                resolved,
                root: denied,
            };
        }
    }

    if allowed_roots
        .iter()
        .any(|root| under_resolved(Path::new(root)))
    {
        DestPolicy::Allowed(resolved)
    } else {
        DestPolicy::NotAllowed(resolved)
    }
}

/// The owner of the link at `path`, `None` when it cannot be read.
fn owner_of_link(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| m.uid())
}

/// An allowed root resolved, but only through links root owns: `/home ->
/// var/home` is the host's layout, while a link a local user owns could be
/// pointed anywhere and carry the policy with it — compared as written, such
/// a root admits only what lies under the link's own path (bd sp78.18).
/// `None` when the root cannot be resolved, or a link on its way is not
/// root's.
fn resolve_root(root: &Path, owner: impl Fn(&Path) -> Option<u32>) -> Option<std::path::PathBuf> {
    resolve_trusting(root, |link| owner(link) == Some(0))
}

/// `path` resolved one name at a time, every link met — in the path as
/// written and inside any link's target — passing `trusted`; `None` when one
/// does not, or the path cannot be resolved.
fn resolve_trusting(root: &Path, trusted: impl Fn(&Path) -> bool) -> Option<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    // realpath, one name at a time, so every link met — in the root as
    // written AND inside any link's target — is judged by its owner.
    let mut pending: std::collections::VecDeque<std::ffi::OsString> =
        std::collections::VecDeque::new();
    if !root.is_absolute() {
        return None;
    }
    for c in root.components() {
        if let Component::Normal(n) = c {
            pending.push_back(n.to_os_string());
        } else if c == Component::ParentDir {
            pending.push_back("..".into());
        }
    }
    let mut out = PathBuf::from("/");
    let mut hops = 0;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 || !trusted(&next) {
                    return None;
                }
                let target = std::fs::read_link(&next).ok()?;
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                let mut front: Vec<std::ffi::OsString> = Vec::new();
                for c in target.components() {
                    match c {
                        Component::Normal(n) => front.push(n.to_os_string()),
                        Component::ParentDir => front.push("..".into()),
                        _ => {}
                    }
                }
                for n in front.into_iter().rev() {
                    pending.push_front(n);
                }
            }
            Ok(_) => out = next,
            // What does not exist yet holds no link: the rest is taken as written.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out = next,
            Err(_) => return None,
        }
    }
    Some(out)
}

/// The directory roots a restore is permitted to read a snapshot FROM.
///
/// Both the configured mount point and whatever the target is *actually*
/// mounted at, because udisks2 mounts the same filesystem at
/// `/run/media/<user>/<label>` while the config names `/mnt/<label>`, and a
/// snapshot path recorded in the index can legitimately be either.
pub fn snapshot_source_roots(config: &crate::config::Config) -> Vec<String> {
    source_roots_with(&config.targets, |t| {
        crate::health::find_any_mount(&t.mount, &t.serial, &t.role)
    })
}

/// [`snapshot_source_roots`] with the "where is this target mounted right now"
/// lookup supplied by the caller, so the list-building can be exercised without
/// consulting the host's mount table.
fn source_roots_with(
    targets: &[crate::config::Target],
    actual_mount: impl Fn(&crate::config::Target) -> Option<String>,
) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for t in targets {
        if !t.mount.is_empty() && !roots.contains(&t.mount) {
            roots.push(t.mount.clone());
        }
        if let Some(actual) = actual_mount(t)
            && !roots.contains(&actual)
        {
            roots.push(actual);
        }
    }
    roots
}

/// Refuse to read a restore SOURCE that is not inside a configured backup target.
///
/// `check_dest_allowed` constrains where a restore may write. Nothing
/// constrained where it may READ: `restore_snapshot` and `restore_files` run as
/// root under the D-Bus helper and took the snapshot path straight from the
/// caller, so an authorized client could have root copy any directory tree on
/// the host — `/etc`, `/root`, another user's home — into a permitted
/// destination it can then read unprivileged (bd DAS-Backup-Manager-7ra).
///
/// Fails closed: an empty root list permits nothing, and the source must exist
/// (a restore from a non-existent snapshot has nothing to do anyway), so the
/// path is fully resolved and a symlinked ancestor cannot smuggle a read past
/// the check.
pub fn check_source_allowed(
    snapshot: &Path,
    source_roots: &[String],
) -> Result<std::path::PathBuf, String> {
    let resolved = snapshot
        .canonicalize()
        .map_err(|e| format!("Cannot resolve snapshot '{}': {e}", snapshot.display()))?;

    if source_roots.is_empty() {
        return Err(format!(
            "Refusing to restore from '{}': no backup targets are configured",
            resolved.display()
        ));
    }

    let permitted = source_roots.iter().any(|root| {
        let root_resolved = Path::new(root)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(root).to_path_buf());
        resolved.starts_with(&root_resolved)
    });

    if permitted {
        Ok(resolved)
    } else {
        Err(format!(
            "Refusing to restore from '{}': not inside any configured backup target ({})",
            resolved.display(),
            source_roots.join(", ")
        ))
    }
}

/// Reject a member path that could escape the snapshot or the destination.
///
/// One check covers both directions, because the same string is joined onto
/// each: a `..` component in `file_path` would otherwise let `src` climb out of
/// the snapshot AND let `dest_file` climb out of the validated destination.
fn safe_member(file_path: &str) -> Result<&Path, String> {
    use std::path::Component;
    let p = Path::new(file_path);
    if p.is_absolute() {
        return Err(format!("'{file_path}' is absolute"));
    }
    for component in p.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!(
                "'{file_path}' contains a non-literal path component"
            ));
        }
    }
    Ok(p)
}

/// A directory held open, reached one name at a time without following a
/// link: every write of a restore goes through one (bd
/// DAS-Backup-Manager-fcmk).
///
/// The policy judges the destination's RESOLVED path, and a write by path
/// resolves it again: a directory on the way that another local user owns
/// can be swapped for a link between the two, and root followed it.
/// `O_NOFOLLOW` on a path guards only its last name. Here each name is
/// opened `O_NOFOLLOW | O_DIRECTORY` relative to the one before, so a link
/// anywhere on the way — there from the start, or swapped in at any moment —
/// is refused (`ELOOP`/`ENOTDIR`), never followed. No write goes through a
/// link: below the destination, one link [`Follow`] admits is resolved and
/// its directory re-opened by that real path the same way (bd mz3z).
struct DirFd(std::os::fd::OwnedFd);

/// When the walk below a destination may follow a link (bd
/// DAS-Backup-Manager-mz3z): only below a destination a user owns (one root
/// owns — made by the restore itself — follows none), only a link that user
/// owns (never root's: the restore makes root-owned links itself, with
/// targets from the snapshot), whose target — every link on its way owned by
/// root or that user — resolves under an `allowed_roots` entry and no denied
/// root, to an existing directory that user owns. That directory is then
/// re-opened from `/` by the strict walk, so the link itself is never
/// traversed and, wherever a rename on the way lands the resolution, root
/// writes only into a directory the destination's owner owns. Any other link
/// is refused, as before.
struct Follow<'a> {
    allowed_roots: &'a [String],
    /// The owner of the destination's own root, held open by the restore.
    dest_owner: u32,
    /// The owner and target of the link `name` in the directory held, read
    /// from one inode (`O_PATH | O_NOFOLLOW`, then `fstat` and `readlinkat`):
    /// never by path, never two lookups a rename could split. A seam.
    link: &'a LinkAt<'a>,
    /// The owner of a link met on the way to a target, by path. A seam.
    path_owner: &'a dyn Fn(&Path) -> Option<u32>,
    /// The owner of the directory a link resolved to, read through its fd. A
    /// seam.
    dir_owner: &'a dyn Fn(&DirFd) -> std::io::Result<u32>,
}

/// Reads the owner and target of the link `name` in a held directory.
type LinkAt<'a> =
    dyn Fn(&DirFd, &std::ffi::CStr) -> std::io::Result<(u32, std::path::PathBuf)> + 'a;

/// How many links one walk below a destination may follow.
const MAX_FOLLOWS: u32 = 8;

impl<'a> Follow<'a> {
    /// The policy for a restore into `dest`, held open: the real owner
    /// lookups.
    fn on_this_host(allowed_roots: &'a [String], dest: &DirFd) -> std::io::Result<Self> {
        let dest_owner = dir_owner_of(dest)?;
        Ok(Self {
            allowed_roots,
            dest_owner,
            link: &link_at,
            path_owner: &owner_of_link,
            dir_owner: &dir_owner_of,
        })
    }
}

/// The owner of the directory held.
fn dir_owner_of(dir: &DirFd) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::File::from(dir.0.try_clone()?).metadata()?.uid())
}

/// The owner and target of the link `name` in `dir`, both from the one inode
/// opened `O_PATH | O_NOFOLLOW` through the directory held.
fn link_at(dir: &DirFd, name: &std::ffi::CStr) -> std::io::Result<(u32, std::path::PathBuf)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: valid dir fd and NUL-terminated name; the fd is owned below.
    let fd = unsafe {
        libc::openat(
            dir.raw(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd was just returned by openat and is owned by nobody else.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid fd, st large enough.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded and filled st.
    let st = unsafe { st.assume_init() };
    if (st.st_mode & libc::S_IFMT) != libc::S_IFLNK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a link",
        ));
    }
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: an O_PATH fd of a link with an empty name reads that link.
    let n = unsafe {
        libc::readlinkat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok((st.st_uid, std::ffi::OsStr::from_bytes(&buf).into()))
}

/// `name` as a C string: one path component, never empty, `.` or `..`.
fn c_name(name: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "'{}' is not a single path component",
                name.to_string_lossy()
            ),
        ));
    }
    std::ffi::CString::new(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// A link met where a directory was expected, said so.
fn refuse_link(e: std::io::Error, at: &Path) -> std::io::Error {
    match e.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => std::io::Error::new(
            e.kind(),
            format!(
                "'{}' is a link or not a directory: a restore writes through no link",
                at.display()
            ),
        ),
        _ => e,
    }
}

impl DirFd {
    fn openat(dir: libc::c_int, name: &std::ffi::CStr) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: a valid directory fd (or AT_FDCWD) and a NUL-terminated name;
        // the fd returned is owned here and closed on drop.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned by openat and is owned by nobody else.
        Ok(Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }))
    }

    fn raw(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    /// The directory `name` under this one, made (mode 0777 less the umask,
    /// as `create_dir_all`) when it is missing and `create` is set.
    fn child(&self, name: &std::ffi::OsStr, create: bool, at: &Path) -> std::io::Result<Self> {
        let c = c_name(name)?;
        self.open_child(&c, create).map_err(|e| refuse_link(e, at))
    }

    /// `name` under this one, made when missing and `create` is set; the raw
    /// error, a link met there being `ELOOP`.
    fn open_child(&self, c: &std::ffi::CStr, create: bool) -> std::io::Result<Self> {
        match Self::openat(self.raw(), c) {
            Err(e) if create && e.raw_os_error() == Some(libc::ENOENT) => {
                // SAFETY: valid dir fd and NUL-terminated name.
                if unsafe { libc::mkdirat(self.raw(), c.as_ptr(), 0o777) } != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::EEXIST) {
                        return Err(e);
                    }
                }
                Self::openat(self.raw(), c)
            }
            r => r,
        }
    }

    /// Whether `name` in this directory is a symbolic link, asked of the
    /// directory held (`fstatat`, `AT_SYMLINK_NOFOLLOW`).
    fn is_link(&self, c: &std::ffi::CStr) -> bool {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid dir fd, NUL-terminated name, st large enough.
        let r = unsafe {
            libc::fstatat(
                self.raw(),
                c.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        // SAFETY: fstatat succeeded and filled st.
        r == 0 && (unsafe { st.assume_init() }.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }

    /// The link `name` in this directory, at `at`, followed by `follow`'s
    /// rule ([`Follow`]): the directory its target resolves to, re-opened from
    /// `/` by the strict walk, and that directory's real path.
    fn follow_link(
        &self,
        c: &std::ffi::CStr,
        at: &Path,
        follow: &Follow,
    ) -> std::io::Result<(Self, std::path::PathBuf)> {
        let refuse = |why: String| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("'{}' is a link {why}: not followed", at.display()),
            )
        };
        if follow.dest_owner == 0 {
            return Err(refuse(
                "below a destination root owns (the restore made it): no link there is followed"
                    .into(),
            ));
        }
        let (owner, target) = (follow.link)(self, c)?;
        if owner != follow.dest_owner {
            return Err(refuse(format!(
                "owned by uid {owner}, not the destination's owner (uid {})",
                follow.dest_owner
            )));
        }
        let target = target.as_path();
        // A relative target is read from the directory the link sits in;
        // `at` is that directory's real path and the link's name.
        let abs = match at.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target.to_path_buf(),
        };
        let trusted = |link: &Path| matches!((follow.path_owner)(link), Some(u) if u == 0 || u == follow.dest_owner);
        let resolved = resolve_trusting(&abs, trusted).ok_or_else(|| {
            refuse(format!(
                "to '{}', and a link on the way there is not root's or the destination \
                 owner's, or it cannot be resolved",
                abs.display()
            ))
        })?;
        match judge_dest(resolved, follow.allowed_roots) {
            DestPolicy::Allowed(real) => {
                let dir = Self::open_abs(&real, false).map_err(|e| {
                    refuse(format!(
                        "to '{}', which cannot be opened as a directory ({e})",
                        real.display()
                    ))
                })?;
                // The destination's owner may point a link only at a directory
                // that is theirs or root's: a restore into another user's home
                // must not let that user steer root's writes into a third
                // user's directory under an allowed root.
                let owner = (follow.dir_owner)(&dir)?;
                if owner != follow.dest_owner {
                    return Err(refuse(format!(
                        "to '{}', a directory owned by uid {owner}, not the destination's \
                         owner (uid {})",
                        real.display(),
                        follow.dest_owner
                    )));
                }
                Ok((dir, real))
            }
            DestPolicy::Denied { resolved, root } => Err(refuse(format!(
                "to '{}', under '{root}', which is never a permitted destination",
                resolved.display()
            ))),
            DestPolicy::NotAllowed(resolved) => Err(refuse(format!(
                "to '{}', not under any [restore] allowed_roots ({}); to restore through \
                 it, add '{}' to [restore] allowed_roots",
                resolved.display(),
                follow.allowed_roots.join(", "),
                resolved.display()
            ))),
        }
    }

    /// The absolute directory `path`, opened from `/` one name at a time;
    /// with `create`, missing directories are made.
    fn open_abs(path: &Path, create: bool) -> std::io::Result<Self> {
        use std::path::Component;
        let mut dir = Self::openat(libc::AT_FDCWD, c"/")?;
        let mut at = std::path::PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    at.push(name);
                    dir = dir.child(name, create, &at)?;
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("'{}' is not an absolute, literal path", path.display()),
                    ));
                }
            }
        }
        Ok(dir)
    }

    /// The directory `rel` (literal names only) under this one, made when
    /// missing. `at` names this directory, for the error.
    fn descend(&self, rel: &Path, at: &Path, follow: &Follow) -> std::io::Result<Self> {
        use std::path::Component;
        // `at` is always the real path of the directory reached: a followed
        // link moves it to where the link resolved.
        let mut at = at.to_path_buf();
        let mut dir: Option<Self> = None;
        let mut follows = 0;
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("'{}' contains a non-literal path component", rel.display()),
                ));
            };
            at.push(name);
            let here = dir.as_ref().unwrap_or(self);
            let c = c_name(name)?;
            let next = match here.open_child(&c, true) {
                // A link there answers ENOTDIR (O_DIRECTORY) or ELOOP
                // (O_NOFOLLOW); only one the held directory says is a link
                // is judged for following.
                Err(e)
                    if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
                        && here.is_link(&c) =>
                {
                    follows += 1;
                    if follows > MAX_FOLLOWS {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!(
                                "'{}': more than {MAX_FOLLOWS} links followed on the way",
                                at.display()
                            ),
                        ));
                    }
                    let (d, real) = here.follow_link(&c, &at, follow)?;
                    at = real;
                    d
                }
                r => r.map_err(|e| refuse_link(e, &at))?,
            };
            dir = Some(next);
        }
        match dir {
            Some(d) => Ok(d),
            // `rel` empty: this directory itself.
            None => Ok(Self(self.0.try_clone()?)),
        }
    }

    /// `name` in this directory opened for writing, created or truncated —
    /// never through a link sitting at `name` (`O_NOFOLLOW`: `ELOOP`).
    fn create_file(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        use std::os::fd::FromRawFd;
        let c = c_name(name)?;
        // SAFETY: valid dir fd and NUL-terminated name; mode as std's create.
        let fd = unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                // O_NONBLOCK: a FIFO planted at `name` refuses (ENXIO) instead
                // of blocking the restore, and its locks, until a reader comes.
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_TRUNC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o666 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned by openat and is owned by nobody else.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    /// A link `name` -> `target` in this directory, replacing whatever
    /// non-directory was there (as the path-based code removed it first).
    fn replace_symlink(&self, target: &Path, name: &std::ffi::OsStr) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let c = c_name(name)?;
        let t = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: valid dir fd and NUL-terminated names. A missing name is
        // fine; any other unlink failure surfaces as symlinkat's EEXIST.
        unsafe { libc::unlinkat(self.raw(), c.as_ptr(), 0) };
        if unsafe { libc::symlinkat(t.as_ptr(), self.raw(), c.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Copy `src` over `dest` without following a symlink at `dest`.
///
/// `std::fs::copy` opens the destination write-truncate, which FOLLOWS a
/// symlink sitting at that path — so a pre-planted link at the destination
/// turned a restore into an overwrite of whatever it pointed at. `O_NOFOLLOW`
/// applies to the final component, which is precisely that case.
fn copy_no_follow(src: &Path, dir: &DirFd, name: &std::ffi::OsStr) -> std::io::Result<u64> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW on the source too: the caller judged it by symlink_metadata,
    // and a link swapped in since must not be read through. O_NONBLOCK so a
    // FIFO cannot block the open; then the OPEN HANDLE must be a regular file,
    // as std::fs::copy required — a FIFO or a device node (a snapshot's
    // dev/zero would be read until the destination filled) is refused.
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)?;
    if !reader.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("'{}' is not a regular file", src.display()),
        ));
    }
    let mut writer = dir.create_file(name)?;
    std::io::copy(&mut reader, &mut writer)
}

/// Restore specific files from a snapshot to a destination.
pub fn restore_files(
    snapshot_path: &Path,
    file_paths: &[&str],
    dest: &Path,
    allowed_roots: &[String],
    allowed_sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let total = file_paths.len() as u64;

    // Destination policy is checked BEFORE anything is created — including the
    // destination directory itself, which `create_dir_all` below would
    // otherwise materialise inside a forbidden root.
    let dest = &check_dest_allowed(dest, allowed_roots)?;

    // Source policy is checked for the same reason and at the same point: the
    // snapshot path arrives from the caller and is read as root.
    let snapshot_root = check_source_allowed(snapshot_path, allowed_sources)?;
    let snapshot_path = snapshot_root.as_path();

    progress.on_stage("Restoring files", total);
    progress.on_log(
        LogLevel::Info,
        &format!("Restoring {} files to {}", total, dest.display()),
    );

    // Every write below goes through this fd: no link on the way is followed,
    // whenever it appears (DirFd).
    let dest_fd = DirFd::open_abs(dest, true)
        .map_err(|e| format!("Cannot open the destination '{}': {e}", dest.display()))?;
    let follow = Follow::on_this_host(allowed_roots, &dest_fd)?;

    let mut files_restored: usize = 0;
    let mut bytes_restored: u64 = 0;
    let mut errors: Vec<String> = Vec::new();

    for (i, file_path) in file_paths.iter().enumerate() {
        // Reject the member path itself. This is the check that actually stops
        // traversal, and unlike the old guard it CANNOT fail open: it inspects
        // the requested string, not whatever the filesystem happens to resolve.
        let member = match safe_member(file_path) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("Path traversal blocked: {e}"));
                continue;
            }
        };
        let src = snapshot_path.join(member);

        // Belt and braces: the member's PARENT must resolve inside the
        // snapshot, so no linked ancestor leads the read out of it. The member
        // itself is not resolved: a regular file is read where its parent is
        // (opened O_NOFOLLOW), and a symlink is recreated from read_link
        // below, its target never read — so where that target lies, or
        // whether it exists on this host, does not decide whether the link is
        // restored (bd 5ln). A parent that does not resolve refuses: realpath
        // can fail where the kernel's own lookup still succeeds.
        let canonical_parent = match src.parent().map(Path::canonicalize) {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                errors.push(format!(
                    "Cannot resolve the parent of '{file_path}' in the snapshot: {e}"
                ));
                continue;
            }
            None => {
                errors.push(format!("Path traversal blocked: '{file_path}'"));
                continue;
            }
        };
        if !canonical_parent.starts_with(&snapshot_root) {
            errors.push(format!("Path traversal blocked: '{}'", file_path));
            continue;
        }

        // The write side: the member's directory, reached and made under
        // the destination's fd with no link followed — planted, restored
        // earlier as a link, or swapped in now (DirFd).
        let (Some(name), Some(rel_dir)) = (member.file_name(), member.parent()) else {
            errors.push(format!("Path traversal blocked: '{file_path}'"));
            continue;
        };
        let dir = match dest_fd.descend(rel_dir, dest, &follow) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("Refusing '{file_path}': {e}"));
                continue;
            }
        };

        // Check if source is a symlink — preserve it as a symlink
        match std::fs::symlink_metadata(&src) {
            Err(e) => {
                errors.push(format!("Cannot stat '{}': {}", file_path, e));
                continue;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                match std::fs::read_link(&src) {
                    Ok(target) => {
                        if let Err(e) = dir.replace_symlink(&target, name) {
                            errors.push(format!("Failed to create symlink '{}': {}", file_path, e));
                            continue;
                        }
                        // A symlink counts as a restored file but has no
                        // payload, so bytes_restored is left alone.
                        files_restored += 1;
                    }
                    Err(e) => {
                        errors.push(format!("Cannot read symlink '{}': {}", file_path, e));
                        continue;
                    }
                }
            }
            Ok(_) => match copy_no_follow(&src, &dir, name) {
                Ok(bytes) => {
                    bytes_restored += bytes;
                    files_restored += 1;
                }
                Err(e) => {
                    errors.push(format!("Failed to copy '{}': {}", file_path, e));
                    continue;
                }
            },
        }

        progress.on_progress(i as u64 + 1, total, file_path);
    }

    let duration_secs = start.elapsed().as_secs();
    let summary = format!(
        "Restored {}/{} files ({} bytes) in {}s, {} error(s)",
        files_restored,
        file_paths.len(),
        bytes_restored,
        duration_secs,
        errors.len()
    );

    progress.on_complete(errors.is_empty(), &summary);
    progress.on_log(LogLevel::Info, &summary);

    Ok(RestoreResult {
        files_restored,
        bytes_restored,
        errors,
        duration_secs,
    })
}

/// Restore an entire snapshot to a destination directory.
pub fn restore_snapshot(
    snapshot_path: &Path,
    dest: &Path,
    allowed_roots: &[String],
    allowed_sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    restore_snapshot_with(
        snapshot_path,
        dest,
        allowed_roots,
        allowed_sources,
        progress,
        Instant::now(),
        try_btrfs_send_receive,
    )
}

/// [`restore_snapshot`] with its start time and its fast path supplied by the
/// caller. The real fast path spawns `btrfs` as root; everything around it —
/// both policy checks, the duration fill-in, the fallback to a recursive copy —
/// is decided here and does not need it.
fn restore_snapshot_with<F>(
    snapshot_path: &Path,
    dest: &Path,
    allowed_roots: &[String],
    allowed_sources: &[String],
    progress: &dyn ProgressCallback,
    start: Instant,
    fast_path: F,
) -> Result<RestoreResult, Box<dyn std::error::Error>>
where
    F: FnOnce(
        &Path,
        &Path,
        &dyn ProgressCallback,
    ) -> Result<RestoreResult, Box<dyn std::error::Error>>,
{
    let dest = &check_dest_allowed(dest, allowed_roots)?;
    let snapshot_resolved = check_source_allowed(snapshot_path, allowed_sources)?;
    let snapshot_path = snapshot_resolved.as_path();

    progress.on_stage("Restoring snapshot", 1);
    progress.on_log(
        LogLevel::Info,
        &format!(
            "Restoring snapshot '{}' to '{}'",
            snapshot_path.display(),
            dest.display()
        ),
    );

    // Made (and checked) the way every write after it is: no link on the way
    // is followed (DirFd). `btrfs receive` gets the directory held open, by
    // its /proc/<pid>/fd/<n> name: the kernel resolves that to this very
    // directory, so a link swapped in on the path since cannot redirect the
    // receive (bd DAS-Backup-Manager-sp78.18). `held` lives until it returns.
    let held = DirFd::open_abs(dest, true)
        .map_err(|e| format!("Cannot open the destination '{}': {e}", dest.display()))?;
    let held_path =
        std::path::PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), held.raw()));

    // Attempt btrfs send | btrfs receive first (fast path for btrfs subvolumes)
    let btrfs_result = fast_path(snapshot_path, &held_path, progress);
    drop(held);

    match btrfs_result {
        Ok(result) => {
            let duration_secs = start.elapsed().as_secs();
            let summary = format!(
                "Snapshot restored via btrfs send/receive ({} bytes) in {}s",
                result.bytes_restored, duration_secs
            );
            progress.on_complete(result.errors.is_empty(), &summary);
            progress.on_log(LogLevel::Info, &summary);
            return Ok(RestoreResult {
                duration_secs,
                ..result
            });
        }
        Err(e) => {
            progress.on_log(
                LogLevel::Warning,
                &format!(
                    "btrfs send/receive not available ({}), falling back to recursive copy",
                    e
                ),
            );
        }
    }

    // Fallback: recursive copy preserving directory structure
    restore_snapshot_recursive(snapshot_path, dest, allowed_roots, &start, progress)
}

/// Try to restore using `btrfs send | btrfs receive`.
fn try_btrfs_send_receive(
    snapshot_path: &Path,
    dest: &Path,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let mut send_child = Command::new("btrfs")
        .args(["send", &snapshot_path.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let send_stdout = send_child
        .stdout
        .take()
        .expect("btrfs send stdout pipe must be present");

    let mut recv_child = Command::new("btrfs")
        .args(["receive", &dest.to_string_lossy()])
        .stdin(send_stdout)
        .stderr(Stdio::null())
        .spawn()?;

    let send_status = send_child.wait()?;
    let recv_status = recv_child.wait()?;

    send_receive_outcome(send_status, recv_status)?;

    progress.on_log(LogLevel::Info, "btrfs send/receive completed successfully");

    // Count bytes from the restored subvolume (best-effort)
    let bytes_restored = count_dir_bytes(dest);

    Ok(RestoreResult {
        files_restored: 0, // not easily countable via send/receive
        bytes_restored,
        errors: Vec::new(),
        duration_secs: 0, // caller fills in
    })
}

/// Judge a finished `btrfs send | btrfs receive` pipeline: BOTH halves must
/// have exited successfully. A receive that succeeded on a truncated stream
/// from a failed send is not a restore, so the send is judged first.
fn send_receive_outcome(
    send_status: std::process::ExitStatus,
    recv_status: std::process::ExitStatus,
) -> Result<(), String> {
    if !send_status.success() {
        return Err(format!("btrfs send exited with {}", send_status));
    }
    if !recv_status.success() {
        return Err(format!("btrfs receive exited with {}", recv_status));
    }
    Ok(())
}

/// Fallback: recursive copy of the snapshot directory tree.
fn restore_snapshot_recursive(
    snapshot_path: &Path,
    dest: &Path,
    allowed_roots: &[String],
    start: &Instant,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    let mut files_restored: usize = 0;
    let mut bytes_restored: u64 = 0;
    let mut errors: Vec<String> = Vec::new();
    let mut file_index: u64 = 0;
    // Every write below goes through this fd (DirFd).
    let dest_fd = DirFd::open_abs(dest, false)
        .map_err(|e| format!("Cannot open the destination '{}': {e}", dest.display()))?;
    let follow = Follow::on_this_host(allowed_roots, &dest_fd)?;

    // Count total entries first for progress reporting (best-effort)
    let total_estimate: u64 = WalkDir::new(snapshot_path)
        .follow_links(false)
        .into_iter()
        .filter(|e| e.as_ref().map(|e| !e.file_type().is_dir()).unwrap_or(false))
        .count() as u64;

    for entry in WalkDir::new(snapshot_path).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("WalkDir error: {}", e));
                continue;
            }
        };

        let rel_path = match entry.path().strip_prefix(snapshot_path) {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!(
                    "strip_prefix error for '{}': {}",
                    entry.path().display(),
                    e
                ));
                continue;
            }
        };

        // Skip the root itself
        if rel_path == Path::new("") {
            continue;
        }

        let dest_path = dest.join(rel_path);
        let file_type = entry.file_type();

        // The directory this entry is, or lands in, reached and made under
        // the destination's fd with no link followed (DirFd).
        if file_type.is_dir() {
            if let Err(e) = dest_fd.descend(rel_path, dest, &follow) {
                errors.push(format!("Refusing '{}': {e}", rel_path.display()));
            }
            continue;
        }
        let (Some(name), Some(rel_dir)) = (rel_path.file_name(), rel_path.parent()) else {
            errors.push(format!("Refusing '{}': no file name", rel_path.display()));
            continue;
        };
        let dir = match dest_fd.descend(rel_dir, dest, &follow) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("Refusing '{}': {e}", rel_path.display()));
                continue;
            }
        };

        if file_type.is_symlink() {
            match std::fs::read_link(entry.path()) {
                Ok(target) => {
                    if let Err(e) = dir.replace_symlink(&target, name) {
                        errors.push(format!(
                            "Failed to create symlink '{}': {}",
                            dest_path.display(),
                            e
                        ));
                        continue;
                    }
                    files_restored += 1;
                    file_index += 1;
                    progress.on_progress(file_index, total_estimate, &rel_path.to_string_lossy());
                }
                Err(e) => {
                    errors.push(format!(
                        "Cannot read symlink '{}': {}",
                        entry.path().display(),
                        e
                    ));
                }
            }
            continue;
        }

        // Regular file.
        //
        // copy_no_follow, NOT std::fs::copy: the latter opens the destination
        // write-truncate, which FOLLOWS a symlink sitting at that path. That is
        // the bd s05 hazard, and this path had been left out of that fix —
        // restore_files used the guarded helper while restore_snapshot did not,
        // even though this is the COMMON path (try_btrfs_send_receive falls back
        // here for any snapshot that is not a receivable read-only subvolume).
        // The D-Bus RestoreSnapshot method runs as root and allows /tmp by
        // default, so a pre-planted link was enough to write snapshot content
        // through it and defeat RESTORE_DENIED_ROOTS entirely.
        // bd DAS-Backup-Manager-nsp (finding #3).
        match copy_no_follow(entry.path(), &dir, name) {
            Ok(bytes) => {
                bytes_restored += bytes;
                files_restored += 1;
                file_index += 1;
                progress.on_progress(file_index, total_estimate, &rel_path.to_string_lossy());
            }
            Err(e) => {
                errors.push(format!(
                    "Failed to copy '{}': {}",
                    entry.path().display(),
                    e
                ));
            }
        }
    }

    let duration_secs = start.elapsed().as_secs();
    let summary = format!(
        "Snapshot restored via recursive copy: {files_restored} files, {bytes_restored} bytes, {} error(s) in {duration_secs}s",
        errors.len()
    );

    progress.on_complete(errors.is_empty(), &summary);
    progress.on_log(LogLevel::Info, &summary);

    Ok(RestoreResult {
        files_restored,
        bytes_restored,
        errors,
        duration_secs,
    })
}

/// Sum the sizes of all regular files under a directory (best-effort, ignores errors).
fn count_dir_bytes(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::TestProgress;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: write a file with given content inside a base dir.
    // --- bd DAS-Backup-Manager-s05 --------------------------------------
    #[test]
    fn dest_policy_refuses_denied_roots_even_if_allowed() {
        // The denylist wins over configuration: listing a system root in
        // allowed_roots must NOT enable it.
        for denied in crate::config::RESTORE_DENIED_ROOTS {
            let err =
                check_dest_allowed(Path::new(denied), &[denied.to_string(), "/tmp".to_string()])
                    .expect_err("{denied} must never be a permitted destination");
            assert!(err.contains("never a permitted destination"), "{err}");
        }
    }

    #[test]
    fn dest_policy_admits_a_srv_subdir_while_still_refusing_the_srv_doc_roots() {
        // bd DAS-Backup-Manager-tku. Backing up /srv/VirtualMachines is useless
        // if it cannot be restored, so it is granted in allowed_roots — but as a
        // SUBDIRECTORY of /srv, never /srv itself, because /srv/http and
        // /srv/ftp are served to the network.
        check_dest_allowed(
            Path::new("/srv/VirtualMachines"),
            &["/srv/VirtualMachines".to_string()],
        )
        .expect("the granted VM subdirectory must be a permitted destination");

        // And the denylist holds even against the widest plausible mistake —
        // someone granting the whole of /srv later.
        for served in ["/srv/http", "/srv/ftp"] {
            let err = check_dest_allowed(Path::new(served), &["/srv".to_string()])
                .expect_err("a served document root must never be a destination");
            assert!(err.contains("never a permitted destination"), "{err}");
        }

        // Component-wise, not string-prefix: a sibling that merely starts with
        // the same characters must not be caught by the denylist.
        check_dest_allowed(Path::new("/srv/http-archive"), &["/srv".to_string()])
            .expect("/srv/http-archive is not /srv/http");
    }

    #[test]
    fn dest_policy_refuses_paths_outside_allowed_roots() {
        let outside = std::env::temp_dir().join("das-restore-outside-test");
        let err = check_dest_allowed(&outside, &["/home".to_string()])
            .expect_err("a temp path is not under /home");
        assert!(err.contains("allowed_roots"), "{err}");
    }

    #[test]
    fn dest_policy_admits_a_path_under_an_allowed_root() {
        let dir = TempDir::new().unwrap();
        // Positive control: without this, the two refusals above would pass
        // even if the function refused everything unconditionally.
        check_dest_allowed(dir.path(), &test_roots()).expect("temp dir must be allowed");
    }

    #[test]
    fn an_allowed_root_reached_through_a_link_is_compared_resolved() {
        // bd DAS-Backup-Manager-sp78.18: the destination is resolved, the root
        // was not — on a host where /home is a link (/home -> var/home) every
        // restore into /home was refused.
        let real = TempDir::new().unwrap();
        let links = TempDir::new().unwrap();
        let root = links.path().join("home");
        std::os::unix::fs::symlink(real.path(), &root).unwrap();
        // Through a link root owns: resolved.
        let root_owned = |_: &Path| Some(0);
        assert_eq!(
            resolve_root(&root, root_owned),
            Some(real.path().canonicalize().unwrap())
        );
        // A root-owned link whose target runs through a user's link: the
        // user's link is judged too (the test's links are the user's own
        // when not root, so: owner 0 only for the first).
        let inner = links.path().join("inner");
        std::os::unix::fs::symlink(real.path(), &inner).unwrap();
        let outer = links.path().join("outer");
        std::os::unix::fs::symlink(&inner, &outer).unwrap();
        let only_outer = |p: &Path| {
            if p == outer.as_path() {
                Some(0)
            } else {
                Some(1000)
            }
        };
        assert_eq!(resolve_root(&outer, only_outer), None);
        assert_eq!(
            resolve_root(&outer, root_owned),
            Some(real.path().canonicalize().unwrap())
        );
        // Through a link a local user owns (this test's own link, when not
        // run as root): not resolved, so a user cannot point a root elsewhere.
        // SAFETY: geteuid() is always safe.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(resolve_root(&root, owner_of_link), None);
            let allowed = [root.to_string_lossy().into_owned()];
            assert!(
                check_dest_allowed(&root.join("restored"), &allowed).is_err(),
                "a user's link does not widen the policy"
            );
        }
    }

    #[test]
    fn member_paths_may_not_escape() {
        for bad in ["../etc/passwd", "a/../../b", "/absolute/path", ".."] {
            assert!(safe_member(bad).is_err(), "{bad} must be rejected");
        }
        // Positive control.
        assert!(safe_member("dir/file.txt").is_ok());
    }

    #[test]
    fn restore_refuses_a_traversing_member_without_writing() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "ok.txt", "fine");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["../escape.txt", "ok.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 1, "only the safe member restores");
        assert!(
            result.errors.iter().any(|e| e.contains("traversal")),
            "{:?}",
            result.errors
        );
        assert!(!dest.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn copy_does_not_follow_a_symlink_planted_at_the_destination() {
        // The concrete attack: a pre-planted link at dest_file turned a restore
        // into an overwrite of whatever it pointed at. `std::fs::copy` follows
        // it; `copy_no_follow` must not.
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "payload.txt", "attacker content");
        let dest = TempDir::new().unwrap();

        let victim = dest.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, dest.path().join("payload.txt")).unwrap();

        let err = copy_to(
            &snap.path().join("payload.txt"),
            &dest.path().join("payload.txt"),
        )
        .expect_err("must refuse to write through a symlink");
        // ELOOP on stable Rust: ErrorKind::FilesystemLoop is still unstable, so
        // assert the raw errno rather than a nightly-only variant.
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "the symlink target must be untouched"
        );
    }

    #[test]
    fn restore_snapshot_recursive_does_not_write_through_a_linked_directory_at_the_destination() {
        // O_NOFOLLOW guards only the final component; a planted link to a
        // DIRECTORY carried create_dir_all and every write under it out of
        // the destination. Since mz3z a link may be followed into a granted
        // root; one into a root that is not granted still refuses.
        let snap = TempDir::new().unwrap();
        write_file(&snap.path().join("door"), "x.txt", "payload");
        let outside = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dest.path().join("door")).unwrap();
        let only_dest = vec![dest.path().canonicalize().unwrap().display().to_string()];

        let result = restore_snapshot_recursive(
            snap.path(),
            dest.path(),
            &only_dest,
            &Instant::now(),
            &crate::progress::NullProgress,
        )
        .expect("the walk itself should complete");

        assert!(
            !outside.path().join("x.txt").exists(),
            "the write must not land outside the destination"
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("not under any [restore] allowed_roots")),
            "the refusal must be REPORTED: {:?}",
            result.errors
        );
    }

    #[test]
    fn restore_snapshot_recursive_does_not_follow_a_planted_symlink() {
        // The isolated copy_no_follow test above passed for months while THIS
        // path still called std::fs::copy — restore_files used the guarded
        // helper and restore_snapshot did not. A guard is only as wide as the
        // paths that call it, so the guarantee has to be asserted at the level
        // an attacker actually reaches. bd DAS-Backup-Manager-nsp (finding #3).
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "payload.txt", "attacker content");
        let dest = TempDir::new().unwrap();

        let victim = dest.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, dest.path().join("payload.txt")).unwrap();

        let result = restore_snapshot_recursive(
            snap.path(),
            dest.path(),
            &test_roots(),
            &Instant::now(),
            &crate::progress::NullProgress,
        )
        .expect("the walk itself should complete");

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "root must not write snapshot content through a pre-planted symlink"
        );
        assert!(
            !result.errors.is_empty(),
            "refusing to follow the link must be REPORTED, not silently skipped"
        );
    }

    /// A restore SOURCE outside every configured target must be refused, and
    /// nothing may be copied.
    ///
    /// Counter-test for bd DAS-Backup-Manager-7ra: `restore_files` runs as root
    /// under the D-Bus helper, so without this the snapshot argument was an
    /// arbitrary root-read — copy `/etc` into an allowed root, then read it
    /// unprivileged.
    #[test]
    fn source_policy_refuses_a_snapshot_outside_configured_targets() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "secret.txt", "sensitive");
        let dest = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let err = restore_files(
            snap.path(),
            &["secret.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(elsewhere.path()),
            &progress,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("not inside any configured backup target"),
            "{err}"
        );
        assert!(
            !dest.path().join("secret.txt").exists(),
            "the refusal must happen before anything is copied"
        );
    }

    /// With no targets configured at all, nothing is a permitted source.
    #[test]
    fn source_policy_fails_closed_with_no_configured_targets() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let err = restore_snapshot(snap.path(), dest.path(), &test_roots(), &[], &progress)
            .unwrap_err()
            .to_string();

        assert!(err.contains("no backup targets are configured"), "{err}");
    }

    /// Positive control for the pair above: the very same call succeeds once the
    /// snapshot's own directory is a permitted source. Without this, the two
    /// refusal tests would still pass if the function refused everything.
    #[test]
    fn source_policy_permits_a_snapshot_inside_a_configured_target() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["a.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 1);
        assert!(dest.path().join("a.txt").exists());
    }

    /// [`copy_no_follow`] onto the path `dest`, its directory opened as a
    /// restore opens it.
    fn copy_to(src: &Path, dest: &Path) -> std::io::Result<u64> {
        let dir = DirFd::open_abs(&dest.parent().unwrap().canonicalize()?, false)?;
        copy_no_follow(src, &dir, dest.file_name().unwrap())
    }

    /// Permit a specific snapshot directory as a restore SOURCE.
    fn test_sources(snap: &Path) -> Vec<String> {
        vec![snap.to_string_lossy().into_owned()]
    }

    /// Allow the platform temp root, wherever TMPDIR points.
    fn test_roots() -> Vec<String> {
        vec![
            std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir())
                .to_string_lossy()
                .into_owned(),
        ]
    }

    fn write_file(base: &Path, rel: &str, content: &str) {
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn browse_entry_is_clone() {
        let entry = BrowseEntry {
            path: "/home/user/file.txt".into(),
            name: "file.txt".into(),
            size: 1024,
            mtime: 1709000000,
            is_dir: false,
        };
        let cloned = entry.clone();
        assert_eq!(cloned.path, "/home/user/file.txt");
        assert_eq!(cloned.size, 1024);
        assert!(!cloned.is_dir);
    }

    #[test]
    fn test_browse_snapshot_lists_files() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "alpha.txt", "hello");
        write_file(snap.path(), "beta.txt", "world");
        fs::create_dir_all(snap.path().join("subdir")).unwrap();

        let entries = browse_snapshot(snap.path(), None).unwrap();

        // We should have subdir + alpha.txt + beta.txt = 3 entries
        assert_eq!(entries.len(), 3);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));
        assert!(names.contains(&"subdir"));
    }

    #[test]
    fn test_browse_snapshot_with_prefix() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "root.txt", "root");
        write_file(snap.path(), "inner/a.txt", "a");
        write_file(snap.path(), "inner/b.txt", "b");

        let entries = browse_snapshot(snap.path(), Some("inner")).unwrap();

        // Only inner/a.txt and inner/b.txt — not root.txt
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(!names.contains(&"root.txt"));
    }

    #[test]
    fn test_browse_snapshot_sorts_dirs_first() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "zebra.txt", "z");
        write_file(snap.path(), "alpha.txt", "a");
        fs::create_dir_all(snap.path().join("middle_dir")).unwrap();
        fs::create_dir_all(snap.path().join("aaa_dir")).unwrap();

        let entries = browse_snapshot(snap.path(), None).unwrap();

        // All directories must precede all files
        let mut saw_file = false;
        for entry in &entries {
            if !entry.is_dir {
                saw_file = true;
            }
            if saw_file && entry.is_dir {
                panic!(
                    "Directory '{}' appeared after a file in sorted output",
                    entry.name
                );
            }
        }

        // Within directories: aaa_dir before middle_dir
        let dir_names: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(dir_names, vec!["aaa_dir", "middle_dir"]);

        // Within files: alpha.txt before zebra.txt
        let file_names: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(file_names, vec!["alpha.txt", "zebra.txt"]);
    }

    #[test]
    fn test_restore_files_copies_correctly() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "hello.txt", "hello world");
        write_file(snap.path(), "data.bin", "binary data here");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["hello.txt", "data.bin"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 2);
        assert!(result.errors.is_empty());

        let restored_hello = fs::read_to_string(dest.path().join("hello.txt")).unwrap();
        assert_eq!(restored_hello, "hello world");

        let restored_data = fs::read_to_string(dest.path().join("data.bin")).unwrap();
        assert_eq!(restored_data, "binary data here");
    }

    #[test]
    fn test_restore_files_preserves_structure() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "docs/guide.txt", "guide content");
        write_file(snap.path(), "docs/nested/deep.txt", "deep content");
        write_file(snap.path(), "root.txt", "root content");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["docs/guide.txt", "docs/nested/deep.txt", "root.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 3);
        assert!(result.errors.is_empty());

        // Verify nested structure preserved
        assert!(dest.path().join("docs/guide.txt").exists());
        assert!(dest.path().join("docs/nested/deep.txt").exists());
        assert!(dest.path().join("root.txt").exists());

        let guide = fs::read_to_string(dest.path().join("docs/guide.txt")).unwrap();
        assert_eq!(guide, "guide content");

        let deep = fs::read_to_string(dest.path().join("docs/nested/deep.txt")).unwrap();
        assert_eq!(deep, "deep content");
    }

    #[test]
    fn test_restore_files_reports_progress() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        write_file(snap.path(), "b.txt", "b");
        write_file(snap.path(), "c.txt", "c");

        let dest = TempDir::new().unwrap();
        let progress = TestProgress::new();

        let result = restore_files(
            snap.path(),
            &["a.txt", "b.txt", "c.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(result.files_restored, 3);
        assert!(result.errors.is_empty());

        // Verify on_stage was called with the right total
        let stages = progress.stages.lock().unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].0, "Restoring files");
        assert_eq!(stages[0].1, 3);

        // Verify on_complete was called with success=true
        let completed = progress.completed.lock().unwrap();
        let (success, _summary) = completed
            .as_ref()
            .expect("on_complete should have been called");
        assert!(*success, "Expected success=true but got false");

        // Verify at least one log message was emitted
        let logs = progress.logs.lock().unwrap();
        assert!(!logs.is_empty(), "Expected at least one log message");
    }

    // --- progress recorder ------------------------------------------------
    //
    // `TestProgress` discards `on_progress`, and the per-file counter is part
    // of what the GUI shows during a restore, so these tests record it.
    #[derive(Default)]
    struct Recorder {
        stages: std::sync::Mutex<Vec<(String, u64)>>,
        steps: std::sync::Mutex<Vec<(u64, u64, String)>>,
        logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
        completed: std::sync::Mutex<Option<(bool, String)>>,
    }

    impl ProgressCallback for Recorder {
        fn on_stage(&self, stage: &str, total_steps: u64) {
            self.stages
                .lock()
                .unwrap()
                .push((stage.to_string(), total_steps));
        }
        fn on_progress(&self, current: u64, total: u64, message: &str) {
            self.steps
                .lock()
                .unwrap()
                .push((current, total, message.to_string()));
        }
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, level: LogLevel, message: &str) {
            self.logs.lock().unwrap().push((level, message.to_string()));
        }
        fn on_complete(&self, success: bool, summary: &str) {
            *self.completed.lock().unwrap() = Some((success, summary.to_string()));
        }
    }

    fn browse_entry(name: &str, is_dir: bool) -> BrowseEntry {
        BrowseEntry {
            path: name.into(),
            name: name.into(),
            size: 0,
            mtime: 0,
            is_dir,
        }
    }

    #[test]
    fn browse_order_puts_directories_first_whatever_their_names() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        // The directory sorts first even when its NAME sorts last, in both
        // argument orders — read_dir order is arbitrary, so a listing test
        // cannot be relied on to present the pair this way round.
        let cases = [
            (("zzz", true), ("aaa", false), Less),
            (("aaa", false), ("zzz", true), Greater),
            (("aaa", true), ("zzz", false), Less),
            (("zzz", false), ("aaa", true), Greater),
            // Same kind: by name, ignoring case.
            (("Beta", true), ("alpha", true), Greater),
            (("alpha", false), ("Beta", false), Less),
            (("Same", false), ("same", false), Equal),
        ];
        for ((an, ad), (bn, bd), want) in cases {
            assert_eq!(
                browse_order(&browse_entry(an, ad), &browse_entry(bn, bd)),
                want,
                "{an} (dir={ad}) vs {bn} (dir={bd})"
            );
        }
    }

    fn target(mount: &str) -> crate::config::Target {
        crate::config::Target {
            label: "t".into(),
            serial: String::new(),
            serials: Vec::new(),
            mount_uuid: None,
            mount: mount.into(),
            role: crate::config::TargetRole::Primary,
            retention: crate::config::Retention::default(),
            display_name: String::new(),
            esp_partuuid: None,
        }
    }

    #[test]
    fn source_roots_list_configured_and_actual_mounts_once_each() {
        // Configured path plus the udisks2-style path the target really sits at.
        let roots = source_roots_with(&[target("/mnt/a")], |_| Some("/run/media/u/a".into()));
        assert_eq!(roots, ["/mnt/a", "/run/media/u/a"]);

        // Mounted exactly where configured: one root, not the same one twice.
        let roots = source_roots_with(&[target("/mnt/a")], |t| Some(t.mount.clone()));
        assert_eq!(roots, ["/mnt/a"]);

        // Not mounted anywhere: the configured path is still a root.
        let roots = source_roots_with(&[target("/mnt/a"), target("/mnt/b")], |_| None);
        assert_eq!(roots, ["/mnt/a", "/mnt/b"]);

        // Two targets sharing a mount path do not duplicate it.
        let roots = source_roots_with(&[target("/mnt/a"), target("/mnt/a")], |_| None);
        assert_eq!(roots, ["/mnt/a"]);
    }

    #[test]
    fn source_roots_never_contain_an_empty_root() {
        // An empty string as a source root would make `Path::new("")` a
        // permitted prefix. A target with no mount configured contributes only
        // where it is actually mounted — or nothing at all.
        let none: Vec<String> = Vec::new();
        assert_eq!(source_roots_with(&[target("")], |_| None), none);
        assert_eq!(
            source_roots_with(&[target("")], |_| Some("/run/media/u/x".into())),
            ["/run/media/u/x"]
        );
        assert_eq!(source_roots_with(&[], |_| Some("/x".into())), none);
    }

    #[test]
    fn snapshot_source_roots_reads_the_targets_from_the_config() {
        // The path does not exist and the serial is empty, so the live-mount
        // lookup answers None without consulting any drive on this host.
        let mut cfg = crate::config::Config::default();
        cfg.targets.clear();
        cfg.targets
            .push(target("/nonexistent-das-restore-test/primary"));
        assert_eq!(
            snapshot_source_roots(&cfg),
            ["/nonexistent-das-restore-test/primary"]
        );
    }

    #[test]
    fn restore_files_counts_files_bytes_and_progress() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "12345");
        write_file(snap.path(), "dir/b.txt", "1234567");
        std::os::unix::fs::symlink("a.txt", snap.path().join("link")).unwrap();
        let dest = TempDir::new().unwrap();
        let progress = Recorder::default();

        let result = restore_files(
            snap.path(),
            &["a.txt", "link", "dir/b.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.files_restored, 3);
        // 5 + 7: the symlink is restored as a link and carries no payload.
        assert_eq!(result.bytes_restored, 12);
        assert_eq!(
            *progress.steps.lock().unwrap(),
            [
                (1, 3, "a.txt".to_string()),
                (2, 3, "link".to_string()),
                (3, 3, "dir/b.txt".to_string()),
            ]
        );
        let (ok, summary) = progress.completed.lock().unwrap().clone().unwrap();
        assert!(ok);
        assert!(
            summary.starts_with("Restored 3/3 files (12 bytes)"),
            "{summary}"
        );
    }

    #[test]
    fn restore_files_preserves_symlinks_as_symlinks() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "real content");
        std::os::unix::fs::symlink("a.txt", snap.path().join("link")).unwrap();
        // A dangling link is still a link worth restoring; copying THROUGH it
        // would have nothing to read.
        std::os::unix::fs::symlink("gone", snap.path().join("dangling")).unwrap();
        let dest = TempDir::new().unwrap();
        // A stale regular file already sitting where the link belongs.
        write_file(dest.path(), "link", "stale");

        let result = restore_files(
            snap.path(),
            &["link", "dangling"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.files_restored, 2);
        assert_eq!(result.bytes_restored, 0);
        for (name, want) in [("link", "a.txt"), ("dangling", "gone")] {
            let restored = dest.path().join(name);
            assert!(
                fs::symlink_metadata(&restored)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name} must be restored as a symlink, not as a copy of its target"
            );
            assert_eq!(fs::read_link(&restored).unwrap(), Path::new(want));
        }
    }

    // --- bd DAS-Backup-Manager-mz3z: which links a restore follows -------

    /// A destination with `door` linked to `target`; the walk below it with
    /// `roots` granted and the owners the seams answer.
    fn walk_through_door(
        dest: &Path,
        roots: &[String],
        link_uid: u32,
        chain_uid: Option<u32>,
        rel: &str,
    ) -> std::io::Result<std::path::PathBuf> {
        walk_through_door_owned(dest, roots, link_uid, chain_uid, None, rel)
    }

    /// [`walk_through_door`], the target directory reported owned by
    /// `target_uid` when given.
    fn walk_through_door_owned(
        dest: &Path,
        roots: &[String],
        link_uid: u32,
        chain_uid: Option<u32>,
        target_uid: Option<u32>,
        rel: &str,
    ) -> std::io::Result<std::path::PathBuf> {
        let dest = dest.canonicalize()?;
        let fd = DirFd::open_abs(&dest, false)?;
        // The real link's target, the owner the test says.
        let link = |d: &DirFd, c: &std::ffi::CStr| link_at(d, c).map(|(_, t)| (link_uid, t));
        let path_owner = |_: &Path| Some(chain_uid.unwrap_or(OWN));
        let dir_owner = |_: &DirFd| Ok(target_uid.unwrap_or(OWN));
        let follow = Follow {
            allowed_roots: roots,
            dest_owner: OWN,
            link: &link,
            path_owner: &path_owner,
            dir_owner: &dir_owner,
        };
        let dir = fd.descend(Path::new(rel), &dest, &follow)?;
        // Where it landed, as the kernel sees the fd.
        fs::read_link(format!("/proc/self/fd/{}", dir.raw()))
    }

    /// The destination's owner in these walks: a user, whoever runs the suite
    /// (CI's container runs it as root).
    const OWN: u32 = 1000;

    fn me() -> u32 {
        OWN
    }

    #[test]
    fn a_link_the_destination_owner_made_into_a_granted_root_is_followed() {
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), dest.path().join("door")).unwrap();
        let landed = walk_through_door(dest.path(), &test_roots(), me(), None, "door/sub").unwrap();
        assert_eq!(landed, target.path().canonicalize().unwrap().join("sub"));
        assert!(target.path().join("sub").is_dir(), "made under the target");
        // A root-owned link is not: the restore makes those itself, its
        // target taken from the snapshot (review of mz3z).
        let err = walk_through_door(dest.path(), &test_roots(), 0, None, "door").unwrap_err();
        assert!(
            err.to_string()
                .contains("owned by uid 0, not the destination's owner"),
            "{err}"
        );
    }

    #[test]
    fn a_link_into_a_root_not_granted_is_refused_and_names_the_grant() {
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), dest.path().join("door")).unwrap();
        let only_dest = vec![dest.path().canonicalize().unwrap().display().to_string()];
        let err = walk_through_door(dest.path(), &only_dest, me(), None, "door").unwrap_err();
        let want = format!(
            "add '{}' to [restore] allowed_roots",
            target.path().canonicalize().unwrap().display()
        );
        assert!(err.to_string().contains(&want), "{err}");
    }

    #[test]
    fn a_link_into_a_denied_root_is_refused_whatever_is_granted() {
        let dest = TempDir::new().unwrap();
        std::os::unix::fs::symlink("/etc", dest.path().join("door")).unwrap();
        let mut roots = test_roots();
        roots.push("/".into());
        let err = walk_through_door(dest.path(), &roots, me(), None, "door").unwrap_err();
        assert!(
            err.to_string().contains("never a permitted destination"),
            "{err}"
        );
        assert!(
            !err.to_string().contains("add '"),
            "no grant can allow it: {err}"
        );
    }

    #[test]
    fn a_link_another_user_owns_is_never_followed() {
        // The fcmk case: a local user owning a directory under the
        // destination plants a link there, into a root that IS granted.
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), dest.path().join("door")).unwrap();
        let err =
            walk_through_door(dest.path(), &test_roots(), 4242, None, "door/sub").unwrap_err();
        assert!(err.to_string().contains("owned by uid 4242"), "{err}");
        assert!(
            !target.path().join("sub").exists(),
            "nothing made through it"
        );
        // A link on the way to the target, owned by another user: refused too.
        let mid = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), mid.path().join("hop")).unwrap();
        std::fs::remove_file(dest.path().join("door")).unwrap();
        std::os::unix::fs::symlink(mid.path().join("hop"), dest.path().join("door")).unwrap();
        let err =
            walk_through_door(dest.path(), &test_roots(), me(), Some(4242), "door").unwrap_err();
        assert!(
            err.to_string()
                .contains("not root's or the destination owner's"),
            "{err}"
        );
    }

    #[test]
    fn a_link_into_another_user_s_directory_is_refused_even_in_a_granted_root() {
        // A restore into bob's home; bob's own link into alice's directory
        // under /home must not steer root's writes there.
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), dest.path().join("door")).unwrap();
        let err = walk_through_door_owned(
            dest.path(),
            &test_roots(),
            me(),
            None,
            Some(4242),
            "door/sub",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("a directory owned by uid 4242"),
            "{err}"
        );
        assert!(!target.path().join("sub").exists(), "nothing made there");
        // Nor root's: a rename on the way could land the resolution there.
        assert!(
            walk_through_door_owned(dest.path(), &test_roots(), me(), None, Some(0), "door")
                .is_err()
        );
    }

    #[test]
    fn below_a_destination_root_owns_no_link_is_followed() {
        // The restore makes a missing destination as root: nobody chose a
        // link under it.
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path(), dest.path().join("door")).unwrap();
        let base = dest.path().canonicalize().unwrap();
        let fd = DirFd::open_abs(&base, false).unwrap();
        let link = |d: &DirFd, c: &std::ffi::CStr| link_at(d, c).map(|(_, t)| (0, t));
        let root_owned = |_: &Path| Some(0);
        let dir_owner = |_: &DirFd| Ok(0);
        let roots = test_roots();
        let follow = Follow {
            allowed_roots: &roots,
            dest_owner: 0,
            link: &link,
            path_owner: &root_owned,
            dir_owner: &dir_owner,
        };
        let Err(err) = fd.descend(Path::new("door"), &base, &follow) else {
            panic!("a link below a root-owned destination was followed");
        };
        assert!(
            err.to_string().contains("below a destination root owns"),
            "{err}"
        );
    }

    #[test]
    fn the_link_s_owner_and_target_come_from_one_inode() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("/somewhere/else", dir.path().join("l")).unwrap();
        fs::write(dir.path().join("f"), "x").unwrap();
        let fd = DirFd::open_abs(&dir.path().canonicalize().unwrap(), false).unwrap();
        let (uid, target) = link_at(&fd, c"l").unwrap();
        // SAFETY: getuid cannot fail.
        assert_eq!(uid, unsafe { libc::getuid() });
        assert_eq!(target, Path::new("/somewhere/else"));
        assert!(link_at(&fd, c"f").is_err(), "a file is not a link");
        assert!(link_at(&fd, c"missing").is_err());
    }

    #[test]
    fn a_dangling_link_or_a_link_to_a_file_is_refused() {
        let dest = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        std::os::unix::fs::symlink(target.path().join("gone"), dest.path().join("door")).unwrap();
        let err =
            walk_through_door(dest.path(), &test_roots(), me(), None, "door/sub").unwrap_err();
        assert!(
            err.to_string().contains("cannot be opened as a directory"),
            "{err}"
        );
        assert!(
            !target.path().join("gone").exists(),
            "never made through a link"
        );
        fs::write(target.path().join("file"), "x").unwrap();
        std::fs::remove_file(dest.path().join("door")).unwrap();
        std::os::unix::fs::symlink(target.path().join("file"), dest.path().join("door")).unwrap();
        assert!(walk_through_door(dest.path(), &test_roots(), me(), None, "door").is_err());
    }

    #[test]
    fn a_relative_link_resolves_from_where_it_sits_and_follows_are_bounded() {
        let dest = TempDir::new().unwrap();
        let base = dest.path().canonicalize().unwrap();
        // dest/l0 -> t0, t0/l1 -> ../t1, ... : each hop relative to its own dir.
        for i in 0..=MAX_FOLLOWS {
            fs::create_dir(base.join(format!("t{i}"))).unwrap();
        }
        std::os::unix::fs::symlink("t0", base.join("l0")).unwrap();
        for i in 1..=MAX_FOLLOWS {
            std::os::unix::fs::symlink(format!("../t{i}"), base.join(format!("t{}/l{i}", i - 1)))
                .unwrap();
        }
        let hops = |n: u32| {
            (0..n)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("/")
        };
        let landed =
            walk_through_door(&base, &test_roots(), me(), None, &hops(MAX_FOLLOWS)).unwrap();
        assert_eq!(landed, base.join(format!("t{}", MAX_FOLLOWS - 1)));
        let err = walk_through_door(&base, &test_roots(), me(), None, &hops(MAX_FOLLOWS + 1))
            .unwrap_err();
        assert!(err.to_string().contains("links followed"), "{err}");
    }

    #[test]
    fn restore_files_writes_through_an_own_link_into_a_granted_root() {
        let snap = TempDir::new().unwrap();
        write_file(&snap.path().join("Music"), "song.flac", "audio");
        let dest = TempDir::new().unwrap();
        let relocated = TempDir::new().unwrap();
        std::os::unix::fs::symlink(relocated.path(), dest.path().join("Music")).unwrap();
        let result = restore_files(
            snap.path(),
            &["Music/song.flac"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &crate::progress::NullProgress,
        )
        .unwrap();
        assert!(
            dest.path().join("Music").is_symlink(),
            "the link itself is left as it was"
        );
        // SAFETY: getuid cannot fail.
        if unsafe { libc::getuid() } == 0 {
            // A suite run as root (CI's container): the destination is
            // root's, so no link below it is followed.
            assert!(!relocated.path().join("song.flac").exists());
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.contains("below a destination root owns")),
                "{:?}",
                result.errors
            );
            return;
        }
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            fs::read_to_string(relocated.path().join("song.flac")).unwrap(),
            "audio"
        );
    }

    #[test]
    fn restore_files_does_not_write_through_a_symlink_planted_at_the_destination() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "payload.txt", "attacker content");
        write_file(snap.path(), "ok.txt", "fine");
        let dest = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, dest.path().join("payload.txt")).unwrap();
        let progress = Recorder::default();

        let result = restore_files(
            snap.path(),
            &["payload.txt", "ok.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        assert_eq!(result.files_restored, 1);
        assert_eq!(result.bytes_restored, 4);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].contains("payload.txt"),
            "{:?}",
            result.errors
        );
        // Positive control: the member with nothing planted really was written.
        assert_eq!(
            fs::read_to_string(dest.path().join("ok.txt")).unwrap(),
            "fine"
        );
        // A refused member reports no progress step; the next one keeps its
        // own position in the request.
        assert_eq!(
            *progress.steps.lock().unwrap(),
            [(2, 2, "ok.txt".to_string())]
        );
        let (ok, _) = progress.completed.lock().unwrap().clone().unwrap();
        assert!(!ok, "a refused member must not be reported as success");
    }

    #[test]
    fn restore_files_refuses_absolute_and_non_literal_members() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "ok.txt", "fine");
        let outside = TempDir::new().unwrap();
        write_file(outside.path(), "secret.txt", "secret");
        let dest = TempDir::new().unwrap();
        let absolute = outside.path().join("secret.txt");

        let result = restore_files(
            snap.path(),
            &[absolute.to_str().unwrap(), "./ok.txt", "ok.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert_eq!(result.files_restored, 1);
        assert_eq!(result.errors.len(), 2, "{:?}", result.errors);
        assert!(result.errors.iter().all(|e| e.contains("traversal")));
        let restored: Vec<_> = fs::read_dir(dest.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(restored, ["ok.txt"], "only the literal member is written");
    }

    #[test]
    fn restore_files_restores_a_link_that_points_outside_as_a_link() {
        // bd 5ln: the link is recreated with read_link, its target never read,
        // so where the target lies (or whether it exists on this host) must not
        // decide whether the link is restored.
        let snap = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_file(outside.path(), "secret.txt", "secret");
        let target = outside.path().join("secret.txt");
        std::os::unix::fs::symlink(&target, snap.path().join("leak")).unwrap();
        let dest = TempDir::new().unwrap();

        let result = restore_files(
            snap.path(),
            &["leak"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.files_restored, 1);
        assert_eq!(
            result.bytes_restored, 0,
            "the target's content is never copied"
        );
        let restored = dest.path().join("leak");
        assert!(
            fs::symlink_metadata(&restored)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&restored).unwrap(), target);
    }

    #[test]
    fn restore_files_refuses_a_member_whose_parent_cannot_be_resolved() {
        // An unresolvable parent must refuse, never skip the containment
        // check: realpath can fail (ENAMETOOLONG past PATH_MAX) where the
        // kernel's own lookup in stat/open still succeeds.
        let snap = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        let result = restore_files(
            snap.path(),
            &["nodir/x.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert_eq!(result.files_restored, 0);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].starts_with("Cannot resolve the parent of 'nodir/x.txt'"),
            "{:?}",
            result.errors
        );
        assert!(!dest.path().join("nodir").exists(), "nothing is created");
    }

    #[test]
    fn restore_files_does_not_write_through_a_linked_directory_at_the_destination() {
        // A link already at the destination (planted, or restored earlier as a
        // link) must not carry a later member's write out of the destination
        // into a root that is not granted (bd mz3z: one into a granted root
        // is followed — see restore_files_writes_through_an_own_link_...).
        let snap = TempDir::new().unwrap();
        write_file(&snap.path().join("door"), "x.txt", "payload");
        let outside = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dest.path().join("door")).unwrap();
        let only_dest = vec![dest.path().canonicalize().unwrap().display().to_string()];

        let result = restore_files(
            snap.path(),
            &["door/x.txt"],
            dest.path(),
            &only_dest,
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert_eq!(result.files_restored, 0);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].contains("not under any [restore] allowed_roots"),
            "{:?}",
            result.errors
        );
        assert!(
            !outside.path().join("x.txt").exists(),
            "the write must not land outside the destination"
        );
    }

    /// A callback that, at the first "on_stage", replaces `swap` (a
    /// directory) with a link to `to`: the swap a local user owning a
    /// directory under the destination can make between the policy check and
    /// the writes (bd DAS-Backup-Manager-fcmk).
    struct SwapAtStage {
        swap: std::path::PathBuf,
        to: std::path::PathBuf,
        done: std::sync::atomic::AtomicBool,
    }

    impl ProgressCallback for SwapAtStage {
        fn on_stage(&self, _: &str, _: u64) {
            if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::fs::remove_dir_all(&self.swap).unwrap();
                std::os::unix::fs::symlink(&self.to, &self.swap).unwrap();
            }
        }
        fn on_progress(&self, _: u64, _: u64, _: &str) {}
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, _: LogLevel, _: &str) {}
        fn on_complete(&self, _: bool, _: &str) {}
    }

    fn swap_at_stage(swap: &Path, to: &Path) -> SwapAtStage {
        SwapAtStage {
            swap: swap.to_path_buf(),
            to: to.to_path_buf(),
            done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn a_link_swapped_in_after_the_check_carries_no_write_out_of_the_destination() {
        // dest = <tmp>/owned/out, `owned` a directory another local user owns
        // and swaps for a link to `outside` once the check has passed.
        for whole_snapshot in [false, true] {
            let snap = TempDir::new().unwrap();
            write_file(&snap.path().join("d"), "x.txt", "payload");
            let base = TempDir::new().unwrap();
            let owned = base.path().join("owned");
            std::fs::create_dir(&owned).unwrap();
            let outside = TempDir::new().unwrap();
            let progress = swap_at_stage(&owned, outside.path());
            let dest = owned.join("out");
            let outcome = if whole_snapshot {
                restore_snapshot_with(
                    snap.path(),
                    &dest,
                    &test_roots(),
                    &test_sources(snap.path()),
                    &progress,
                    Instant::now(),
                    |_, _, _| Err("no btrfs here".into()),
                )
            } else {
                restore_files(
                    snap.path(),
                    &["d/x.txt"],
                    &dest,
                    &test_roots(),
                    &test_sources(snap.path()),
                    &progress,
                )
            };
            let leaked: Vec<_> = WalkDir::new(outside.path())
                .min_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.path().to_path_buf())
                .collect();
            assert!(
                leaked.is_empty(),
                "whole snapshot {whole_snapshot}: written outside the destination: {leaked:?}"
            );
            let refused = match &outcome {
                Err(e) => e.to_string(),
                Ok(r) => r.errors.join("; "),
            };
            assert!(
                refused.contains("link"),
                "whole snapshot {whole_snapshot}: the refusal is reported: {refused:?}"
            );
        }
    }

    #[test]
    fn copy_no_follow_refuses_a_source_that_is_not_a_regular_file() {
        // std::fs::copy refused these; its hand-written replacement did not.
        // A FIFO blocked the open forever (holding the restore's locks); a
        // device node such as dev/zero would be read until the destination
        // filled. Run on a thread so the unfixed code fails, not hangs.
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let out = dir.path().join("out");
        let (tx, rx) = std::sync::mpsc::channel();
        let (f, o) = (fifo.clone(), out.clone());
        std::thread::spawn(move || {
            let _ = tx.send(copy_to(&f, &o).map_err(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("copying a FIFO must refuse at once, not block");
        let err = got.expect_err("a FIFO is not a regular file");
        assert!(err.contains("not a regular file"), "{err}");
        assert!(!out.exists(), "nothing is written");
    }

    #[test]
    fn copy_no_follow_does_not_read_through_a_symlink_source() {
        // The member is judged a regular file by symlink_metadata, then opened:
        // a link swapped in between must not be read through.
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "real.txt", "secret");
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link")).unwrap();
        let out = dir.path().join("out.txt");

        assert!(copy_to(&dir.path().join("link"), &out).is_err());
        assert!(
            !out.exists(),
            "nothing is written when the source is a link"
        );
    }

    #[test]
    fn restore_files_refuses_a_member_reached_through_a_linked_directory() {
        // The containment check that matters: an ANCESTOR of the member is a
        // link leading out of the snapshot, so reading the member reads outside.
        let snap = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_file(outside.path(), "secret.txt", "secret");
        std::os::unix::fs::symlink(outside.path(), snap.path().join("door")).unwrap();
        let dest = TempDir::new().unwrap();

        let result = restore_files(
            snap.path(),
            &["door/secret.txt"],
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();

        assert_eq!(result.files_restored, 0);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].contains("traversal"),
            "{:?}",
            result.errors
        );
        assert!(!dest.path().join("door").exists(), "nothing is written");
    }

    #[test]
    fn restore_files_creates_nothing_under_a_refused_destination() {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "a");
        let parent = TempDir::new().unwrap();
        let allowed = parent.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        // A link INSIDE the allowed root that leads out of it: the policy is
        // applied after resolution, so the write must be refused.
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), allowed.join("door")).unwrap();
        let roots = vec![
            allowed
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ];

        let err = restore_files(
            snap.path(),
            &["a.txt"],
            &allowed.join("door/new"),
            &roots,
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("allowed_roots"), "{err}");
        assert!(!outside.path().join("new").exists());

        // Positive control: a not-yet-existing directory honestly under the
        // root is created and written.
        let result = restore_files(
            snap.path(),
            &["a.txt"],
            &allowed.join("fresh/new"),
            &roots,
            &test_sources(snap.path()),
            &Recorder::default(),
        )
        .unwrap();
        assert_eq!(result.files_restored, 1);
        assert_eq!(
            fs::read_to_string(allowed.join("fresh/new/a.txt")).unwrap(),
            "a"
        );
    }

    /// Two files, one symlink and more directories than non-directories, so a
    /// progress total that counted the wrong kind of entry cannot agree.
    fn snapshot_tree() -> TempDir {
        let snap = TempDir::new().unwrap();
        write_file(snap.path(), "a.txt", "123");
        write_file(snap.path(), "sub/b.txt", "12345");
        std::os::unix::fs::symlink("a.txt", snap.path().join("link")).unwrap();
        fs::create_dir_all(snap.path().join("empty/nested")).unwrap();
        snap
    }

    fn assert_tree_restored(dest: &Path) {
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "123");
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "12345");
        assert!(dest.join("empty/nested").is_dir());
        let link = dest.join("link");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("a.txt"));
    }

    #[test]
    fn recursive_restore_counts_files_bytes_and_progress() {
        let snap = snapshot_tree();
        let dest = TempDir::new().unwrap();
        let progress = Recorder::default();

        let result = restore_snapshot_recursive(
            snap.path(),
            dest.path(),
            &test_roots(),
            &Instant::now(),
            &progress,
        )
        .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.files_restored, 3, "two files and the symlink");
        assert_eq!(result.bytes_restored, 8, "3 + 5; the link has no payload");
        assert_tree_restored(dest.path());

        // One step per non-directory entry, numbered 1..=3 out of 3, whatever
        // order the walk visits them in.
        let steps = progress.steps.lock().unwrap();
        let counters: Vec<(u64, u64)> = steps.iter().map(|(c, t, _)| (*c, *t)).collect();
        assert_eq!(counters, [(1, 3), (2, 3), (3, 3)]);
        let mut names: Vec<&str> = steps.iter().map(|(_, _, m)| m.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.txt", "link", "sub/b.txt"]);

        let (ok, summary) = progress.completed.lock().unwrap().clone().unwrap();
        assert!(ok);
        assert!(
            summary.contains("3 files, 8 bytes, 0 error(s)"),
            "{summary}"
        );
    }

    #[test]
    fn restore_snapshot_fills_in_the_duration_of_a_fast_path_restore() {
        let snap = snapshot_tree();
        let dest = TempDir::new().unwrap();
        let progress = Recorder::default();
        // The fast path cannot know when the restore began (it reports 0 and
        // leaves the caller to fill it in), so a restore that started 90 s ago
        // must not come back claiming to have taken no time.
        let started = Instant::now()
            .checked_sub(std::time::Duration::from_secs(90))
            .expect("the monotonic clock is more than 90 s old");

        let result = restore_snapshot_with(
            snap.path(),
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
            started,
            |_, _, _| {
                Ok(RestoreResult {
                    files_restored: 0,
                    bytes_restored: 4096,
                    errors: Vec::new(),
                    duration_secs: 0,
                })
            },
        )
        .unwrap();

        assert!(
            (90..150).contains(&result.duration_secs),
            "duration_secs = {}",
            result.duration_secs
        );
        assert_eq!(result.bytes_restored, 4096);
        // The fast path succeeded, so the recursive copy must not have run too.
        assert!(!dest.path().join("a.txt").exists());
        let (ok, summary) = progress.completed.lock().unwrap().clone().unwrap();
        assert!(ok);
        assert!(
            summary.contains("btrfs send/receive (4096 bytes)"),
            "{summary}"
        );
    }

    /// bd DAS-Backup-Manager-sp78.18 (1): `btrfs receive` was handed the
    /// destination's path, which a link swapped in after the walk could
    /// redirect. It gets the directory the walk holds open, by its
    /// `/proc/<pid>/fd/<n>` name, which the kernel resolves to that directory
    /// whatever the path now leads to.
    #[test]
    fn the_fast_path_is_given_the_held_directory_not_the_path() {
        let snap = snapshot_tree();
        let dest = TempDir::new().unwrap();
        let seen = std::sync::Mutex::new(None);
        restore_snapshot_with(
            snap.path(),
            dest.path(),
            &test_roots(),
            &test_sources(snap.path()),
            &crate::progress::NullProgress,
            Instant::now(),
            |_, given, _| {
                *seen.lock().unwrap() = Some((given.to_path_buf(), given.canonicalize().ok()));
                Ok(RestoreResult {
                    files_restored: 0,
                    bytes_restored: 0,
                    errors: Vec::new(),
                    duration_secs: 0,
                })
            },
        )
        .unwrap();
        let (given, resolves_to) = seen.lock().unwrap().clone().expect("the fast path ran");
        assert!(given.starts_with("/proc/"), "{given:?}");
        assert_eq!(resolves_to, Some(dest.path().canonicalize().unwrap()));
    }

    #[test]
    fn restore_snapshot_falls_back_to_a_recursive_copy_when_the_fast_path_fails() {
        let snap = snapshot_tree();
        let parent = TempDir::new().unwrap();
        let dest = parent.path().join("not/yet/there");
        let progress = Recorder::default();

        let result = restore_snapshot_with(
            snap.path(),
            &dest,
            &test_roots(),
            &test_sources(snap.path()),
            &progress,
            Instant::now(),
            |_, _, _| Err("btrfs send exited with exit status: 1".into()),
        )
        .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.files_restored, 3);
        assert_eq!(result.bytes_restored, 8);
        assert_tree_restored(&dest);
        // The reason for the fallback must reach the operator.
        let logs = progress.logs.lock().unwrap();
        assert!(
            logs.iter().any(|(level, msg)| *level == LogLevel::Warning
                && msg.contains("exit status: 1")
                && msg.contains("falling back to recursive copy")),
            "{logs:?}"
        );
    }

    #[test]
    fn restore_snapshot_checks_policy_before_the_fast_path_runs() {
        let snap = snapshot_tree();
        let dest = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let ran = std::cell::Cell::new(false);
        let fast_path = |_: &Path, _: &Path, _: &dyn ProgressCallback| {
            ran.set(true);
            Err("unreachable".into())
        };

        let err = restore_snapshot_with(
            snap.path(),
            dest.path(),
            &test_roots(),
            &test_sources(elsewhere.path()),
            &Recorder::default(),
            Instant::now(),
            fast_path,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("not inside any configured backup target"),
            "{err}"
        );
        assert!(
            !ran.get(),
            "nothing may be read before the source is permitted"
        );
        assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[test]
    fn send_receive_needs_both_halves_to_succeed() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        // wait(2) status words: 0 is a clean exit, 1 << 8 is `exit 1`.
        let ok = ExitStatus::from_raw(0);
        let failed = ExitStatus::from_raw(1 << 8);
        assert!(ok.success() && !failed.success(), "fixture sanity");

        assert_eq!(send_receive_outcome(ok, ok), Ok(()));

        let err = send_receive_outcome(failed, ok).unwrap_err();
        assert!(err.starts_with("btrfs send exited with"), "{err}");

        let err = send_receive_outcome(ok, failed).unwrap_err();
        assert!(err.starts_with("btrfs receive exited with"), "{err}");

        // Both failed: the send is the cause, the receive only its consequence.
        let err = send_receive_outcome(failed, failed).unwrap_err();
        assert!(err.starts_with("btrfs send exited with"), "{err}");
    }

    #[test]
    fn count_dir_bytes_sums_regular_files_only() {
        let dir = snapshot_tree();
        // 3 + 5. Directories and the symlink (which is not followed, or a.txt
        // would be counted twice) contribute nothing.
        assert_eq!(count_dir_bytes(dir.path()), 8);

        let empty = TempDir::new().unwrap();
        assert_eq!(count_dir_bytes(empty.path()), 0);
        assert_eq!(count_dir_bytes(&empty.path().join("absent")), 0);
    }
}
