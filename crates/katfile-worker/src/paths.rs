//! Filesystem operations on the staging volume shared with SFTPGo.
//!
//! Everything below the configured roots is reached through directory file
//! descriptors (`openat`/`linkat`/`renameat`/`unlinkat`) with `O_NOFOLLOW`, so a path
//! component that is (or becomes) a symlink can never redirect an operation outside
//! a user's home. Layout (one filesystem, required for hard links and renames):
//!
//! ```text
//! <users_dir>/<username>/...           SFTPGo homes (visible to users)
//! <spool_dir>/<job-uuid>               immutable hard-link snapshot of an upload
//! <spool_dir>/quarantine/              transit area for verified deletions
//! <spool_dir>/deleted-homes/<pid>/     homes of deleted users, out of SFTPGo's reach
//! <temp_dir>/.sftpgo-upload.*          SFTPGo atomic-upload temp files
//! ```

use std::collections::HashSet;
use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use serde::Serialize;
use tracing::{debug, info, warn};

const QUARANTINE_DIR: &str = "quarantine";
const DELETED_HOMES_DIR: &str = "deleted-homes";
const SFTPGO_TEMP_PREFIX: &str = ".sftpgo-upload.";

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// Untrusted input that can never be valid (bad username or path component).
    #[error("invalid path: {0}")]
    Invalid(String),
    /// The filesystem layout is unsafe (symlink or non-directory where a directory is expected).
    #[error("unsafe path: {0}")]
    Unsafe(String),
    #[error("not found")]
    NotFound,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<Errno> for PathError {
    fn from(e: Errno) -> Self {
        PathError::Io(e.into())
    }
}

/// Identity of a staged file. While a snapshot link exists its inode cannot be
/// reused, so (dev, ino) identifies the content exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    /// Inode change time: set by the kernel, cannot be forged by clients (unlike mtime).
    pub ctime_ns: i64,
    pub nlink: u64,
}

impl Identity {
    // `Stat` field types differ between Linux and macOS; the casts keep both building.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(st: &Stat) -> Identity {
        Identity {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size: st.st_size as u64,
            mtime_ns: (st.st_mtime as i64) * 1_000_000_000 + st.st_mtime_nsec as i64,
            ctime_ns: (st.st_ctime as i64) * 1_000_000_000 + st.st_ctime_nsec as i64,
            nlink: st.st_nlink as u64,
        }
    }

    pub fn same_inode(&self, other: &Identity) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }

    /// Same inode with unchanged size and modification time.
    pub fn unchanged(&self, other: &Identity) -> bool {
        self.same_inode(other) && self.size == other.size && self.mtime_ns == other.mtime_ns
    }
}

/// Validated path relative to a user's home (from an SFTPGo `virtual_path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPath {
    parts: Vec<String>,
}

impl RelPath {
    /// Parse an absolute virtual path such as `/photos/2026/a.jpg`.
    pub fn parse_virtual(vp: &str) -> Result<RelPath, PathError> {
        if vp.len() > 4096 {
            return Err(PathError::Invalid("path longer than 4096 bytes".into()));
        }
        let rest = vp.strip_prefix('/').ok_or_else(|| PathError::Invalid("virtual path must be absolute".into()))?;
        let parts: Vec<String> = rest.split('/').map(str::to_owned).collect();
        RelPath::from_parts(parts)
    }

    pub fn from_parts(parts: Vec<String>) -> Result<RelPath, PathError> {
        if parts.is_empty() {
            return Err(PathError::Invalid("empty path".into()));
        }
        for p in &parts {
            validate_component(p)?;
        }
        Ok(RelPath { parts })
    }

    pub fn file_name(&self) -> &str {
        self.parts.last().map(String::as_str).unwrap_or("")
    }

    pub fn dir_parts(&self) -> &[String] {
        &self.parts[..self.parts.len() - 1]
    }

    /// Parent directory relative to the home, `""` for the home itself.
    pub fn rel_dir(&self) -> String {
        self.dir_parts().join("/")
    }

    pub fn virtual_path(&self) -> String {
        format!("/{}", self.parts.join("/"))
    }
}

fn validate_component(c: &str) -> Result<(), PathError> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(PathError::Invalid(format!("illegal path component {c:?}")));
    }
    if c.len() > 255 {
        return Err(PathError::Invalid("path component longer than 255 bytes".into()));
    }
    if c.chars().any(|ch| ch == '/' || ch == '\0' || ch.is_control()) {
        return Err(PathError::Invalid("path component contains a separator or control character".into()));
    }
    Ok(())
}

/// SFTPGo naming rules 6 (lower-case, URI-unreserved); rejects anything else.
pub fn validate_username(u: &str) -> Result<(), PathError> {
    let ok = (1..=64).contains(&u.len())
        && u.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && u.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._~-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(PathError::Invalid(format!("unsupported username {:?}", u.escape_debug().to_string())))
    }
}

/// Which tree holds the visible copy of a file.
#[derive(Debug, Clone)]
pub enum HomeRef<'a> {
    /// `<users_dir>/<username>`.
    User(&'a str),
    /// `<spool_dir>/deleted-homes/<principal_id>`.
    Deleted(&'a str),
}

/// Outcome of snapshotting a staged upload.
#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotResult {
    Created(Identity),
    /// The file is gone (deleted or renamed before the event was processed).
    Missing,
    /// The file at the path no longer matches the event (replaced by a newer upload).
    SizeMismatch(u64),
    NotRegularFile,
    /// The path changed between the check and the link; the link was undone.
    Raced,
}

/// Result of removing visible copies of a snapshot.
#[derive(Debug, Default, Serialize)]
pub struct VisibleCleanup {
    pub removed: u32,
    /// Files that appeared at a path during removal and were put back / left in quarantine.
    pub kept_other: u32,
    pub stranded: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Staging {
    pub users_dir: PathBuf,
    pub spool_dir: PathBuf,
    pub temp_dir: PathBuf,
}

impl Staging {
    pub fn new(users_dir: PathBuf, spool_dir: PathBuf, temp_dir: PathBuf) -> Staging {
        Staging { users_dir, spool_dir, temp_dir }
    }

    /// Create worker-private directories and verify the single-filesystem requirement.
    pub fn prepare(&self) -> Result<(), PathError> {
        let spool = open_root(&self.spool_dir)?;
        for d in [QUARANTINE_DIR, DELETED_HOMES_DIR] {
            match rustix::fs::mkdirat(&spool, d, Mode::from_raw_mode(0o750)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            open_dir_nofollow(spool.as_fd(), d)?;
        }
        let users = open_root(&self.users_dir)?;
        let (a, b) = (rustix::fs::fstat(&users)?, rustix::fs::fstat(&spool)?);
        if a.st_dev != b.st_dev {
            return Err(PathError::Unsafe(
                "users and spool directories are on different filesystems (hard links and renames would fail)".into(),
            ));
        }
        Ok(())
    }

    /// Free bytes available to unprivileged writers on the staging filesystem.
    pub fn free_bytes(&self) -> Result<u64, PathError> {
        let s = rustix::fs::statvfs(&self.spool_dir)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }

    fn open_home(&self, home: &HomeRef<'_>) -> Result<OwnedFd, PathError> {
        match home {
            HomeRef::User(username) => {
                validate_username(username)?;
                let users = open_root(&self.users_dir)?;
                open_dir_nofollow(users.as_fd(), username)
            }
            HomeRef::Deleted(principal_id) => {
                validate_component(principal_id)?;
                let spool = open_root(&self.spool_dir)?;
                let deleted = open_dir_nofollow(spool.as_fd(), DELETED_HOMES_DIR)?;
                open_dir_nofollow(deleted.as_fd(), principal_id)
            }
        }
    }

    /// Walk to the directory containing `rel` without following symlinks.
    fn open_parent(&self, home: &HomeRef<'_>, rel: &RelPath) -> Result<OwnedFd, PathError> {
        let mut dir = self.open_home(home)?;
        for part in rel.dir_parts() {
            dir = open_dir_nofollow(dir.as_fd(), part)?;
        }
        Ok(dir)
    }

    /// Hard-link a completed upload into the spool as `<spool>/<job_id>`.
    pub fn snapshot(
        &self,
        username: &str,
        rel: &RelPath,
        expected_size: u64,
        job_id: &str,
    ) -> Result<SnapshotResult, PathError> {
        validate_component(job_id)?;
        let parent = match self.open_parent(&HomeRef::User(username), rel) {
            Ok(fd) => fd,
            Err(PathError::NotFound) => return Ok(SnapshotResult::Missing),
            Err(e) => return Err(e),
        };
        let name = rel.file_name();
        let st = match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(SnapshotResult::Missing),
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(st.st_mode as _) != FileType::RegularFile {
            return Ok(SnapshotResult::NotRegularFile);
        }
        let before = Identity::from_stat(&st);
        if before.size != expected_size {
            return Ok(SnapshotResult::SizeMismatch(before.size));
        }
        let spool = open_root(&self.spool_dir)?;
        match rustix::fs::linkat(&parent, name, &spool, job_id, AtFlags::empty()) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(SnapshotResult::Missing),
            Err(Errno::XDEV) => return Err(PathError::Unsafe("spool is on a different filesystem".into())),
            Err(e) => return Err(e.into()),
        }
        let linked = rustix::fs::statat(&spool, job_id, AtFlags::SYMLINK_NOFOLLOW)?;
        let after = Identity::from_stat(&linked);
        if FileType::from_raw_mode(linked.st_mode as _) != FileType::RegularFile || !after.same_inode(&before) {
            // The name was swapped between stat and link: undo and let the newer event handle it.
            rustix::fs::unlinkat(&spool, job_id, AtFlags::empty())?;
            return Ok(SnapshotResult::Raced);
        }
        rustix::fs::fsync(&spool)?;
        debug!(job_id, ino = after.ino, size = after.size, "snapshot created");
        Ok(SnapshotResult::Created(after))
    }

    /// Open a snapshot read-only together with its current identity.
    pub fn open_snapshot(&self, job_id: &str) -> Result<(std::fs::File, Identity), PathError> {
        validate_component(job_id)?;
        let spool = open_root(&self.spool_dir)?;
        let fd = match rustix::fs::openat(
            &spool,
            job_id,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Err(PathError::NotFound),
            Err(Errno::LOOP) => return Err(PathError::Unsafe("snapshot is a symlink".into())),
            Err(e) => return Err(e.into()),
        };
        let st = rustix::fs::fstat(&fd)?;
        if FileType::from_raw_mode(st.st_mode as _) != FileType::RegularFile {
            return Err(PathError::Unsafe("snapshot is not a regular file".into()));
        }
        Ok((std::fs::File::from(fd), Identity::from_stat(&st)))
    }

    pub fn snapshot_identity(&self, job_id: &str) -> Result<Option<Identity>, PathError> {
        validate_component(job_id)?;
        let spool = open_root(&self.spool_dir)?;
        match rustix::fs::statat(&spool, job_id, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(Identity::from_stat(&st))),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove the spool link if it still is the job's snapshot. Idempotent.
    pub fn release_snapshot(&self, job_id: &str, expected: &Identity) -> Result<bool, PathError> {
        validate_component(job_id)?;
        let spool = open_root(&self.spool_dir)?;
        let st = match rustix::fs::statat(&spool, job_id, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !Identity::from_stat(&st).same_inode(expected) {
            return Err(PathError::Unsafe(format!("spool entry {job_id} is not the recorded snapshot")));
        }
        rustix::fs::unlinkat(&spool, job_id, AtFlags::empty())?;
        rustix::fs::fsync(&spool)?;
        Ok(true)
    }

    /// Delete `parent/name` only if it is the snapshot's inode.
    ///
    /// The entry is first renamed into the quarantine (atomic), then compared; a
    /// different file is restored with `linkat`, which never overwrites a newer file.
    fn remove_if_same(
        &self,
        parent: BorrowedFd<'_>,
        name: &str,
        ident: &Identity,
        tag: &str,
        out: &mut VisibleCleanup,
    ) -> Result<(), PathError> {
        let spool = open_root(&self.spool_dir)?;
        let quarantine = open_dir_nofollow(spool.as_fd(), QUARANTINE_DIR)?;
        match rustix::fs::renameat(parent, name, &quarantine, tag) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let st = rustix::fs::statat(&quarantine, tag, AtFlags::SYMLINK_NOFOLLOW)?;
        if Identity::from_stat(&st).same_inode(ident) {
            rustix::fs::unlinkat(&quarantine, tag, AtFlags::empty())?;
            out.removed += 1;
            return Ok(());
        }
        match rustix::fs::linkat(&quarantine, tag, parent, name, AtFlags::empty()) {
            Ok(()) => {
                rustix::fs::unlinkat(&quarantine, tag, AtFlags::empty())?;
                out.kept_other += 1;
                warn!(name, "a different file replaced the archived one during cleanup; kept it");
            }
            Err(Errno::EXIST) => {
                warn!(name, tag, "cannot restore a replaced file; left in quarantine for review");
                out.stranded.push(tag.to_owned());
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Remove every visible copy (hard link) of an archived snapshot inside `home`.
    ///
    /// The recorded path is tried first; if other links remain (the user renamed the
    /// file), the home tree is searched for the same inode.
    pub fn remove_visible_copies(
        &self,
        home: &HomeRef<'_>,
        rel: &RelPath,
        ident: &Identity,
        job_id: &str,
    ) -> Result<VisibleCleanup, PathError> {
        validate_component(job_id)?;
        let mut out = VisibleCleanup::default();
        // Leftovers of an interrupted earlier run: exact while the snapshot still exists.
        self.resolve_quarantine_leftovers(job_id, ident, &mut out)?;
        match self.open_parent(home, rel) {
            Ok(parent) => self.remove_if_same(parent.as_fd(), rel.file_name(), ident, &unique_tag(job_id), &mut out)?,
            Err(PathError::NotFound) | Err(PathError::Unsafe(_)) => {}
            Err(e) => return Err(e),
        }
        // nlink counts the spool link itself; anything above one is another visible copy.
        let remaining = self.snapshot_identity(job_id)?.map(|i| i.nlink).unwrap_or(1);
        if remaining > 1 {
            for rel_copy in self.find_links(home, ident, 50_000)? {
                let parent = match self.open_parent(home, &rel_copy) {
                    Ok(p) => p,
                    Err(PathError::NotFound) => continue,
                    Err(e) => return Err(e),
                };
                self.remove_if_same(parent.as_fd(), rel_copy.file_name(), ident, &unique_tag(job_id), &mut out)?;
            }
        }
        Ok(out)
    }

    /// Quarantine entries named `<job_id>.*`: our own inode is removed, anything else is reported.
    fn resolve_quarantine_leftovers(
        &self,
        job_id: &str,
        ident: &Identity,
        out: &mut VisibleCleanup,
    ) -> Result<(), PathError> {
        let spool = open_root(&self.spool_dir)?;
        let quarantine = open_dir_nofollow(spool.as_fd(), QUARANTINE_DIR)?;
        let prefix = format!("{job_id}.");
        for (name, _) in list_names(quarantine.as_fd())? {
            if !name.starts_with(&prefix) {
                continue;
            }
            let st = rustix::fs::statat(&quarantine, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
            if Identity::from_stat(&st).same_inode(ident) {
                rustix::fs::unlinkat(&quarantine, name.as_str(), AtFlags::empty())?;
                out.removed += 1;
            } else {
                out.stranded.push(name);
            }
        }
        Ok(())
    }

    /// Paths inside `home` that are hard links to `ident`'s inode.
    fn find_links(&self, home: &HomeRef<'_>, ident: &Identity, max_entries: usize) -> Result<Vec<RelPath>, PathError> {
        let files = self.scan(home, max_entries)?;
        Ok(files.files.into_iter().filter(|(_, i)| i.same_inode(ident)).map(|(r, _)| r).collect())
    }

    /// Move a deleted user's home out of SFTPGo's reach (atomic rename, no copy).
    pub fn quarantine_home(&self, username: &str, principal_id: &str) -> Result<bool, PathError> {
        validate_username(username)?;
        validate_component(principal_id)?;
        let users = open_root(&self.users_dir)?;
        let spool = open_root(&self.spool_dir)?;
        let deleted = open_dir_nofollow(spool.as_fd(), DELETED_HOMES_DIR)?;
        let st = match rustix::fs::statat(&users, username, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(st.st_mode as _) != FileType::Directory {
            return Err(PathError::Unsafe(format!("home of {username} is not a directory")));
        }
        rustix::fs::renameat(&users, username, &deleted, principal_id)?;
        rustix::fs::fsync(&users)?;
        rustix::fs::fsync(&deleted)?;
        info!(username, principal_id, "home moved out of SFTPGo into deleted-homes quarantine");
        Ok(true)
    }

    /// A username was re-created before the old account's deletion was processed, so
    /// both generations share one home. Files whose inode predates the new account
    /// (`ctime < cutoff`) are moved into the old principal's quarantine, keeping their
    /// relative paths; newer files stay with the new account. Returns (moved, kept).
    pub fn split_recreated_home(
        &self,
        username: &str,
        old_principal_id: &str,
        cutoff_ns: i64,
    ) -> Result<(u32, u32), PathError> {
        validate_username(username)?;
        validate_component(old_principal_id)?;
        let scan = self.scan(&HomeRef::User(username), 200_000)?;
        let spool = open_root(&self.spool_dir)?;
        let deleted = open_dir_nofollow(spool.as_fd(), DELETED_HOMES_DIR)?;
        let target_root = ensure_dir(deleted.as_fd(), old_principal_id)?;
        let (mut moved, mut kept) = (0u32, 0u32);
        for (rel, ident) in scan.files {
            if ident.ctime_ns >= cutoff_ns {
                kept += 1;
                continue;
            }
            let src_parent = match self.open_parent(&HomeRef::User(username), &rel) {
                Ok(fd) => fd,
                Err(PathError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            let mut dst_parent = rustix::fs::openat(
                &target_root,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            for part in rel.dir_parts() {
                dst_parent = ensure_dir(dst_parent.as_fd(), part)?;
            }
            // Never overwrite inside the quarantine: link (fails on EEXIST) then verify.
            match rustix::fs::linkat(&src_parent, rel.file_name(), &dst_parent, rel.file_name(), AtFlags::empty()) {
                Ok(()) => {}
                Err(Errno::EXIST) => {
                    warn!(path = %rel.virtual_path(), "quarantine already has this path; leaving the file in place");
                    kept += 1;
                    continue;
                }
                Err(Errno::NOENT) => continue,
                Err(e) => return Err(e.into()),
            }
            let linked = rustix::fs::statat(&dst_parent, rel.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
            if !Identity::from_stat(&linked).same_inode(&ident) {
                // Replaced between scan and link: that is a new account's file, undo.
                rustix::fs::unlinkat(&dst_parent, rel.file_name(), AtFlags::empty())?;
                kept += 1;
                continue;
            }
            // The old inode is safe in the quarantine; drop the visible name only if unchanged.
            let mut out = VisibleCleanup::default();
            self.remove_if_same(src_parent.as_fd(), rel.file_name(), &ident, &unique_tag(old_principal_id), &mut out)?;
            moved += 1;
            kept += out.kept_other;
        }
        info!(username, old_principal_id, moved, kept, "split re-created home by inode change time");
        Ok((moved, kept))
    }

    /// Remove a quarantined home once it holds no files: only empty directories are
    /// deleted (bottom-up), never file content. Returns true when it is gone.
    pub fn prune_deleted_home(&self, principal_id: &str) -> Result<bool, PathError> {
        validate_component(principal_id)?;
        let spool = open_root(&self.spool_dir)?;
        let deleted = open_dir_nofollow(spool.as_fd(), DELETED_HOMES_DIR)?;
        let home = match open_dir_nofollow(deleted.as_fd(), principal_id) {
            Ok(fd) => fd,
            Err(PathError::NotFound) => return Ok(true),
            Err(e) => return Err(e),
        };
        if !remove_empty_tree(home.as_fd())? {
            return Ok(false);
        }
        match rustix::fs::unlinkat(&deleted, principal_id, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => Ok(true),
            Err(Errno::NOTEMPTY) | Err(Errno::EXIST) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Principal ids that still have a quarantined home.
    pub fn deleted_homes(&self) -> Result<Vec<String>, PathError> {
        let spool = open_root(&self.spool_dir)?;
        let deleted = open_dir_nofollow(spool.as_fd(), DELETED_HOMES_DIR)?;
        Ok(list_names(deleted.as_fd())?
            .into_iter()
            .filter(|(_, t)| *t == FileType::Directory)
            .map(|(n, _)| n)
            .collect())
    }

    /// Regular files of a home (no symlink following), bounded by `max_entries`.
    pub fn scan(&self, home: &HomeRef<'_>, max_entries: usize) -> Result<ScanResult, PathError> {
        let root = match self.open_home(home) {
            Ok(fd) => fd,
            Err(PathError::NotFound) => return Ok(ScanResult::default()),
            Err(e) => return Err(e),
        };
        let mut out = ScanResult::default();
        let mut stack: Vec<(OwnedFd, Vec<String>)> = vec![(root, Vec::new())];
        let mut seen = 0usize;
        while let Some((dir, prefix)) = stack.pop() {
            for (name, ftype) in list_names(dir.as_fd())? {
                seen += 1;
                if seen > max_entries {
                    out.truncated = true;
                    return Ok(out);
                }
                let mut parts = prefix.clone();
                parts.push(name.clone());
                match ftype {
                    FileType::Directory => match open_dir_nofollow(dir.as_fd(), &name) {
                        Ok(fd) => stack.push((fd, parts)),
                        Err(PathError::NotFound) => {}
                        Err(e) => return Err(e),
                    },
                    FileType::RegularFile => {
                        let st = match rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(st) => st,
                            Err(Errno::NOENT) => continue,
                            Err(e) => return Err(e.into()),
                        };
                        match RelPath::from_parts(parts) {
                            Ok(rel) => out.files.push((rel, Identity::from_stat(&st))),
                            Err(_) => out.skipped += 1,
                        }
                    }
                    _ => out.skipped += 1,
                }
            }
        }
        Ok(out)
    }

    /// Spool snapshots that no job claims (crash between link and DB insert).
    pub fn orphan_snapshots(&self, known: &HashSet<String>) -> Result<Vec<(String, Identity)>, PathError> {
        let spool = open_root(&self.spool_dir)?;
        let mut out = Vec::new();
        for (name, ftype) in list_names(spool.as_fd())? {
            if ftype != FileType::RegularFile || known.contains(&name) {
                continue;
            }
            if let Ok(st) = rustix::fs::statat(&spool, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                out.push((name, Identity::from_stat(&st)));
            }
        }
        Ok(out)
    }

    /// Remove an orphan spool link (only when another link keeps the content alive).
    pub fn remove_orphan_snapshot(&self, name: &str, expected: &Identity) -> Result<bool, PathError> {
        validate_component(name)?;
        let spool = open_root(&self.spool_dir)?;
        let st = match rustix::fs::statat(&spool, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let now = Identity::from_stat(&st);
        if !now.same_inode(expected) || now.nlink < 2 {
            return Ok(false);
        }
        rustix::fs::unlinkat(&spool, name, AtFlags::empty())?;
        Ok(true)
    }

    /// Delete SFTPGo atomic-upload temp files untouched for `older_than` (left behind
    /// when SFTPGo is killed mid-upload). Active uploads keep their mtime fresh.
    pub fn sweep_stale_temp(&self, older_than: Duration, now_ms: i64) -> Result<(u64, u64), PathError> {
        let temp = open_root(&self.temp_dir)?;
        let cutoff_ns = (now_ms - older_than.as_millis() as i64) * 1_000_000;
        let (mut files, mut bytes) = (0u64, 0u64);
        for (name, ftype) in list_names(temp.as_fd())? {
            if ftype != FileType::RegularFile || !name.starts_with(SFTPGO_TEMP_PREFIX) {
                continue;
            }
            let st = match rustix::fs::statat(&temp, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(Errno::NOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            let ident = Identity::from_stat(&st);
            if ident.mtime_ns < cutoff_ns {
                rustix::fs::unlinkat(&temp, name.as_str(), AtFlags::empty())?;
                info!(file = %name, bytes = ident.size, "removed stale SFTPGo upload temp file");
                files += 1;
                bytes += ident.size;
            }
        }
        Ok((files, bytes))
    }
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<(RelPath, Identity)>,
    /// Non-regular entries and names that are not valid UTF-8 path components.
    pub skipped: u64,
    pub truncated: bool,
}

/// Open (creating if needed) a child directory without following symlinks.
fn ensure_dir(dir: BorrowedFd<'_>, name: &str) -> Result<OwnedFd, PathError> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o750)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    open_dir_nofollow(dir, name)
}

/// Unique quarantine name for one removal attempt of a job's visible copy.
fn unique_tag(job_id: &str) -> String {
    format!("{job_id}.{}", uuid::Uuid::new_v4().simple())
}

/// Open a trusted root directory (the root itself may be a symlink, e.g. /var on macOS).
fn open_root(path: &Path) -> Result<OwnedFd, PathError> {
    match rustix::fs::openat(CWD, path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => Err(PathError::NotFound),
        Err(e) => Err(e.into()),
    }
}

/// Open a child directory, refusing symlinks and non-directories.
fn open_dir_nofollow(dir: BorrowedFd<'_>, name: &str) -> Result<OwnedFd, PathError> {
    match rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => Err(PathError::NotFound),
        Err(Errno::LOOP) | Err(Errno::NOTDIR) => {
            Err(PathError::Unsafe(format!("{name:?} is a symlink or not a directory")))
        }
        Err(e) => Err(e.into()),
    }
}

/// Directory entries (without `.`/`..`) as UTF-8 names with their type.
fn list_names(dir: BorrowedFd<'_>) -> Result<Vec<(String, FileType)>, PathError> {
    let mut out = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let raw: &CStr = entry.file_name();
        let bytes = raw.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let Ok(name) = std::str::from_utf8(bytes) else {
            warn!("skipping non-UTF-8 directory entry");
            continue;
        };
        let mut ftype = entry.file_type();
        if ftype == FileType::Unknown {
            ftype = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => FileType::from_raw_mode(st.st_mode as _),
                Err(_) => continue,
            };
        }
        out.push((name.to_owned(), ftype));
    }
    Ok(out)
}

/// Remove all empty sub-directories; returns true if `dir` itself is now empty.
fn remove_empty_tree(dir: BorrowedFd<'_>) -> Result<bool, PathError> {
    let mut empty = true;
    for (name, ftype) in list_names(dir)? {
        if ftype == FileType::Directory {
            let child = open_dir_nofollow(dir, &name)?;
            if remove_empty_tree(child.as_fd())? {
                match rustix::fs::unlinkat(dir, name.as_str(), AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => continue,
                    Err(Errno::NOTEMPTY) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        empty = false;
    }
    Ok(empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Fixture {
        _dir: tempfile::TempDir,
        staging: Staging,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["data", "spool", "uploads-tmp"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        let staging = Staging::new(root.join("data"), root.join("spool"), root.join("uploads-tmp"));
        staging.prepare().unwrap();
        Fixture { _dir: dir, staging }
    }

    fn write(path: &Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    fn rel(p: &str) -> RelPath {
        RelPath::parse_virtual(p).unwrap()
    }

    #[test]
    fn virtual_path_validation() {
        assert_eq!(rel("/photos/2026/a.jpg").rel_dir(), "photos/2026");
        assert_eq!(rel("/a.jpg").rel_dir(), "");
        for bad in ["photos/a.jpg", "/", "/a//b", "/../alice/x", "/a/./b", "/a/\u{0}b", "/a/\nb"] {
            assert!(RelPath::parse_virtual(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(RelPath::parse_virtual("/相片 ü/a b.jpg").is_ok());
    }

    #[test]
    fn username_validation() {
        for ok in ["alice", "bob-2", "a.b_c~d", "0x"] {
            assert!(validate_username(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Alice", "../x", ".hidden", "a/b", "ünï", &"x".repeat(65)] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_is_an_immutable_hard_link() {
        let f = fixture();
        let file = f.staging.users_dir.join("alice/photos/a.jpg");
        write(&file, b"hello world");
        let res = f.staging.snapshot("alice", &rel("/photos/a.jpg"), 11, "job1").unwrap();
        let SnapshotResult::Created(id) = res else { panic!("{res:?}") };
        assert_eq!(id.size, 11);
        assert_eq!(id.nlink, 2);
        // The user deletes the visible file: the snapshot keeps the content.
        std::fs::remove_file(&file).unwrap();
        let (mut snap, now) = f.staging.open_snapshot("job1").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut snap, &mut s).unwrap();
        assert_eq!(s, "hello world");
        assert_eq!(now.nlink, 1);
        assert!(f.staging.release_snapshot("job1", &id).unwrap());
        assert!(!f.staging.release_snapshot("job1", &id).unwrap(), "release is idempotent");
    }

    #[test]
    fn snapshot_reports_missing_and_replaced_files() {
        let f = fixture();
        assert_eq!(f.staging.snapshot("alice", &rel("/x.bin"), 1, "j").unwrap(), SnapshotResult::Missing);
        write(&f.staging.users_dir.join("alice/x.bin"), b"newer content");
        assert_eq!(f.staging.snapshot("alice", &rel("/x.bin"), 3, "j").unwrap(), SnapshotResult::SizeMismatch(13));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        let f = fixture();
        write(&f.staging.users_dir.join("bob/secret.txt"), b"bob's data");
        std::fs::create_dir_all(f.staging.users_dir.join("alice")).unwrap();
        // alice/link -> ../bob (directory symlink) and alice/s.txt -> bob/secret.txt (file symlink)
        std::os::unix::fs::symlink("../bob", f.staging.users_dir.join("alice/link")).unwrap();
        std::os::unix::fs::symlink("../bob/secret.txt", f.staging.users_dir.join("alice/s.txt")).unwrap();
        let err = f.staging.snapshot("alice", &rel("/link/secret.txt"), 10, "j1").unwrap_err();
        assert!(matches!(err, PathError::Unsafe(_)), "{err:?}");
        assert_eq!(f.staging.snapshot("alice", &rel("/s.txt"), 10, "j2").unwrap(), SnapshotResult::NotRegularFile);
        // A symlinked home is refused as well.
        std::os::unix::fs::symlink("bob", f.staging.users_dir.join("mallory")).unwrap();
        assert!(matches!(f.staging.snapshot("mallory", &rel("/secret.txt"), 10, "j3"), Err(PathError::Unsafe(_))));
    }

    #[test]
    fn cleanup_removes_only_the_archived_inode() {
        let f = fixture();
        let path = f.staging.users_dir.join("alice/a.txt");
        write(&path, b"archived");
        let SnapshotResult::Created(id) = f.staging.snapshot("alice", &rel("/a.txt"), 8, "j1").unwrap() else {
            panic!()
        };
        // The user deletes and re-uploads a different file under the same name.
        std::fs::remove_file(&path).unwrap();
        write(&path, b"new upload!");
        let out = f.staging.remove_visible_copies(&HomeRef::User("alice"), &rel("/a.txt"), &id, "j1").unwrap();
        assert_eq!(out.removed, 0);
        assert_eq!(out.kept_other, 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"new upload!", "newer file must survive");
    }

    #[test]
    fn cleanup_follows_renamed_copies_by_inode() {
        let f = fixture();
        write(&f.staging.users_dir.join("alice/a.txt"), b"archived");
        let SnapshotResult::Created(id) = f.staging.snapshot("alice", &rel("/a.txt"), 8, "j1").unwrap() else {
            panic!()
        };
        std::fs::create_dir_all(f.staging.users_dir.join("alice/moved")).unwrap();
        std::fs::rename(f.staging.users_dir.join("alice/a.txt"), f.staging.users_dir.join("alice/moved/b.txt"))
            .unwrap();
        let out = f.staging.remove_visible_copies(&HomeRef::User("alice"), &rel("/a.txt"), &id, "j1").unwrap();
        assert_eq!(out.removed, 1);
        assert!(!f.staging.users_dir.join("alice/moved/b.txt").exists());
        assert!(f.staging.release_snapshot("j1", &id).unwrap());
    }

    #[test]
    fn deleted_home_is_quarantined_and_pruned_when_empty() {
        let f = fixture();
        write(&f.staging.users_dir.join("carol/docs/a.txt"), b"x");
        let SnapshotResult::Created(id) = f.staging.snapshot("carol", &rel("/docs/a.txt"), 1, "j1").unwrap() else {
            panic!()
        };
        assert!(f.staging.quarantine_home("carol", "pid-1").unwrap());
        assert!(!f.staging.users_dir.join("carol").exists(), "a recreated user must start with an empty home");
        assert!(!f.staging.prune_deleted_home("pid-1").unwrap(), "files remain, nothing is pruned");
        let out = f.staging.remove_visible_copies(&HomeRef::Deleted("pid-1"), &rel("/docs/a.txt"), &id, "j1").unwrap();
        assert_eq!(out.removed, 1);
        assert!(f.staging.prune_deleted_home("pid-1").unwrap());
        assert_eq!(f.staging.deleted_homes().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn recreated_home_is_split_by_inode_change_time() {
        let f = fixture();
        write(&f.staging.users_dir.join("erin/old/a.txt"), b"previous account");
        std::thread::sleep(Duration::from_millis(20));
        let cutoff = crate::util::now_ms() * 1_000_000;
        std::thread::sleep(Duration::from_millis(20));
        write(&f.staging.users_dir.join("erin/new.txt"), b"new account");
        let (moved, kept) = f.staging.split_recreated_home("erin", "old-pid", cutoff).unwrap();
        assert_eq!((moved, kept), (1, 1));
        assert!(!f.staging.users_dir.join("erin/old/a.txt").exists());
        assert!(f.staging.users_dir.join("erin/new.txt").exists());
        assert!(f.staging.spool_dir.join("deleted-homes/old-pid/old/a.txt").exists());
    }

    #[test]
    fn scan_lists_regular_files_only() {
        let f = fixture();
        write(&f.staging.users_dir.join("dave/a/b/c.txt"), b"1");
        write(&f.staging.users_dir.join("dave/top.txt"), b"22");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", f.staging.users_dir.join("dave/evil")).unwrap();
        let res = f.staging.scan(&HomeRef::User("dave"), 1000).unwrap();
        let mut names: Vec<String> = res.files.iter().map(|(r, _)| r.virtual_path()).collect();
        names.sort();
        assert_eq!(names, vec!["/a/b/c.txt", "/top.txt"]);
        assert_eq!(res.skipped, 1);
    }

    #[test]
    fn stale_temp_files_are_swept() {
        let f = fixture();
        write(&f.staging.temp_dir.join(".sftpgo-upload.abc.big.bin"), b"partial");
        write(&f.staging.temp_dir.join("unrelated.txt"), b"keep");
        let later = crate::util::now_ms() + 2 * 3_600_000;
        let (n, bytes) = f.staging.sweep_stale_temp(Duration::from_secs(3600), later).unwrap();
        assert_eq!((n, bytes), (1, 7));
        assert!(f.staging.temp_dir.join("unrelated.txt").exists());
        // Fresh files are left alone.
        write(&f.staging.temp_dir.join(".sftpgo-upload.def.x"), b"active");
        assert_eq!(f.staging.sweep_stale_temp(Duration::from_secs(3600), crate::util::now_ms()).unwrap(), (0, 0));
    }
}
