use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{ErrorKind, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotIdentity {
    uuid: [u8; 16],
    generation: u64,
    ctransid: u64,
    change_time: (u64, u32),
}

// Linux BTRFS_IOC_GET_SUBVOL_INFO ABI, from linux/btrfs.h. Unlike
// `btrfs subvolume show`, this does not require privileged tree searches.
#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: u64,
    nsec: u32,
}
#[repr(C)]
struct SubvolumeInfo {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: Timespec,
    otime: Timespec,
    stime: Timespec,
    rtime: Timespec,
    reserved: [u64; 8],
}

pub(crate) fn identity(path: &Path) -> Result<SnapshotIdentity> {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.ino() == 256,
        "input is not a Btrfs subvolume root"
    );
    const _: () = assert!(std::mem::size_of::<SubvolumeInfo>() == 504);
    // SAFETY: this C ABI structure consists only of integer fields and arrays.
    let mut info: SubvolumeInfo = unsafe { std::mem::zeroed() };
    let request = libc::_IOR::<SubvolumeInfo>(0x94, 60);
    // SAFETY: the live fd and writable, correctly sized ioctl buffer remain
    // valid for the duration of this synchronous kernel call.
    if unsafe { libc::ioctl(file.as_raw_fd(), request, &mut info) } < 0 {
        return Err(std::io::Error::last_os_error()).context("reading Btrfs snapshot identity");
    }
    // GET_SUBVOL_INFO returns root-item flags (BTRFS_ROOT_SUBVOL_RDONLY=1),
    // not the BTRFS_SUBVOL_RDONLY=2 flag used by SNAP_CREATE_V2.
    ensure!(
        info.flags & 1 != 0,
        "input must be a read-only Btrfs snapshot: {}",
        path.display()
    );
    Ok(SnapshotIdentity {
        uuid: info.uuid,
        generation: info.generation,
        ctransid: info.ctransid,
        change_time: (info.ctime.sec, info.ctime.nsec),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub kind: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub links: u64,
    pub md5: Option<String>,
    pub target: Option<PathBuf>,
    pub sha256: Option<String>,
    pub size: u64,
    pub mtime: Option<(i64, i64)>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub write_flags: u32,
}

pub(crate) fn command(program: &str, args: &[&Path]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("starting {program}"))?;
    ensure!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

pub(crate) fn relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe root-relative path: {}",
        path.display()
    );
    Ok(())
}

pub(crate) fn ancestors(root: &Path, relative_path: &Path) -> Result<()> {
    relative(relative_path)?;
    let mut p = root.to_path_buf();
    for part in relative_path.parent().unwrap().components() {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) => ensure!(
                m.is_dir() && !m.is_symlink(),
                "non-directory ancestor: {}",
                p.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) fn frozen(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path).context("opening snapshot")?;
    ensure!(
        path != Path::new("/"),
        "the live / cannot be a snapshot input"
    );
    identity(&path)?;
    Ok(path)
}

pub(crate) fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(destination).is_err_and(|e| e.kind() == ErrorKind::NotFound),
        "snapshot destination must not exist"
    );
    command(
        "/usr/bin/btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            source,
            destination,
        ],
    )
}

/// Own exactly one new working subvolume, never an input or a discovered tree.
/// Btrfs deletion cannot recurse through nested subvolumes. Interrupted jobs may
/// leave this private directory; it is never a candidate without a receipt.
pub(crate) struct WorkingRoot {
    directory: PathBuf,
    root: PathBuf,
}

impl WorkingRoot {
    pub(crate) fn new(source: &Path, destination: &Path) -> Result<Self> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = tempfile::Builder::new()
            .prefix(".pond-rebase-")
            .tempdir_in(parent)?
            .keep();
        let root = directory.join("root");
        let work = Self { directory, root };
        snapshot(source, &work.root)?;
        Ok(work)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for WorkingRoot {
    fn drop(&mut self) {
        if self.root.exists() {
            let _ = command(
                "/usr/bin/btrfs",
                &[Path::new("subvolume"), Path::new("delete"), &self.root],
            );
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

/// Work stays under an explicitly private name until a sealed snapshot is published.
/// On failure the caller may garbage-collect that private directory later; it
/// must never discover candidates by enumerating temporary subvolumes.
pub(crate) fn publish(source: &Path, destination: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct SnapshotArgs {
        fd: i64,
        transid: u64,
        flags: u64,
        reserved: [u64; 4],
        name: [u8; 4040],
    }
    const _: () = assert!(std::mem::size_of::<SnapshotArgs>() == 4096);
    let source = fs::File::open(source)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::File::open(parent)?;
    let name = destination
        .file_name()
        .context("candidate has no name")?
        .as_bytes();
    ensure!(
        name.len() <= 255 && !name.contains(&0),
        "invalid candidate name"
    );
    // SAFETY: the ioctl argument contains only integer fields and byte arrays.
    let mut args: SnapshotArgs = unsafe { std::mem::zeroed() };
    args.fd = source.as_raw_fd().into();
    args.flags = 2; // BTRFS_SUBVOL_RDONLY; no async creation.
    args.name[..name.len()].copy_from_slice(name);
    // A single snapshot ioctl publishes an already-read-only tree or EEXIST.
    // The CLI's interpretation of an existing directory as a parent is unsafe
    // for this operation; pass its parent fd and exact basename instead.
    // SAFETY: both fds and the correctly sized C ABI buffer live through ioctl.
    if unsafe {
        libc::ioctl(
            parent.as_raw_fd(),
            libc::_IOW::<SnapshotArgs>(0x94, 23),
            &mut args,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error())
            .context("publishing read-only candidate without replacement");
    }
    parent.sync_all()?;
    Ok(())
}

/// One read, two digests. SHA-256 is the change detector (mtime can be reset
/// and MD5 collisions are constructible); MD5 exists only to reproduce
/// libalpm's backup-file decisions.
fn digests(path: &Path) -> Result<(String, String)> {
    use sha2::{Digest, Sha256};
    let mut strong = Sha256::new();
    let mut backup = md5::Context::new();
    let mut file = fs::File::open(path)?;
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        strong.update(&buf[..n]);
        backup.consume(&buf[..n]);
    }
    Ok((
        format!("{:x}", backup.compute()),
        format!("{:x}", strong.finalize()),
    ))
}

pub(crate) fn entry(path: &Path) -> Result<Entry> {
    read_entry(path, true)
}

pub(crate) fn inspect(path: &Path) -> Result<Entry> {
    read_entry(path, false)
}

fn read_entry(path: &Path, contents: bool) -> Result<Entry> {
    let m = fs::symlink_metadata(path)?;
    let mut flags: libc::c_long = 0;
    if m.is_file() || m.is_dir() {
        use std::os::fd::AsRawFd;
        let file = fs::File::open(path)?;
        // FS_IOC_GETFLAGS uses a long-sized ioctl encoding, but returns an int.
        // Zero-initialize the larger buffer so both ABI widths are safe.
        // SAFETY: the live fd and writable buffer remain valid during ioctl.
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                libc::_IOR::<libc::c_long>(b'f'.into(), 1),
                &mut flags,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("reading filesystem write flags");
        }
    }
    let (md5, sha256) = if contents && m.is_file() {
        let (m, s) = digests(path)?;
        (Some(m), Some(s))
    } else {
        (None, None)
    };
    Ok(Entry {
        kind: if m.is_file() {
            "file"
        } else if m.is_dir() {
            "dir"
        } else if m.is_symlink() {
            "link"
        } else {
            "other"
        }
        .into(),
        mode: m.mode(),
        uid: m.uid(),
        gid: m.gid(),
        links: if m.is_dir() { 1 } else { m.nlink() },
        md5,
        sha256,
        size: if m.is_file() { m.len() } else { 0 },
        target: if m.is_symlink() {
            Some(fs::read_link(path)?)
        } else {
            None
        },
        mtime: if m.is_file() || m.is_symlink() {
            Some((m.mtime(), m.mtime_nsec()))
        } else {
            None
        },
        xattrs: attributes(path)?,
        // These flags change write/removal semantics, not just physical layout.
        write_flags: flags as u32 & (0x10 | 0x20),
    })
}

pub(crate) fn optional_entry(root: &Path, path: &Path) -> Result<Option<Entry>> {
    ancestors(root, path)?;
    match entry(&root.join(path)) {
        Ok(entry) => Ok(Some(entry)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn mount_id(path: &Path) -> Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statx is a C structure of integer fields; zero is a valid initial
    // value. The pathname and writable result live through this synchronous call.
    let mut metadata: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
            libc::STATX_MNT_ID,
            &mut metadata,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error()).context("checking snapshot mount boundary");
    }
    ensure!(
        metadata.stx_mask & libc::STATX_MNT_ID != 0,
        "kernel does not expose mount IDs"
    );
    Ok(metadata.stx_mnt_id)
}

pub(super) fn walk_tree(root: &Path, mut visit: impl FnMut(&Path) -> Result<()>) -> Result<()> {
    fn walk(
        path: &Path,
        device: u64,
        mount: u64,
        visit: &mut impl FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        for child in fs::read_dir(path)? {
            let p = child?.path();
            let m = fs::symlink_metadata(&p)?;
            // A nested subvolume becomes an empty inode-2 stub in a snapshot.
            // Btrfs send omits it too. This is not a mounted live subtree.
            if m.is_dir() && m.ino() == 2 && mount_id(&p)? == mount {
                ensure!(
                    fs::read_dir(&p)?.next().is_none(),
                    "nonempty Btrfs snapshot stub"
                );
                continue;
            }
            ensure!(
                m.dev() == device && m.ino() != 256 && mount_id(&p)? == mount,
                "nested mount or subvolume is not part of a snapshot: {}",
                p.display()
            );
            visit(&p)?;
            if m.is_dir() {
                walk(&p, device, mount, visit)?;
            }
        }
        Ok(())
    }
    walk(root, fs::metadata(root)?.dev(), mount_id(root)?, &mut visit)
}

/// Validate isolation without unnecessarily hashing an already completed native
/// output. Mount IDs also catch same-filesystem bind mounts that st_dev misses.
pub(crate) fn check_tree(root: &Path) -> Result<()> {
    walk_tree(root, |_| Ok(()))
}

pub(crate) fn manifest(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    let mut out = BTreeMap::new();
    walk_tree(root, |path| {
        out.insert(path.strip_prefix(root)?.into(), entry(path)?);
        Ok(())
    })?;
    Ok(out)
}

pub(crate) fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let src = match fs::symlink_metadata(source) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if src.as_ref().is_some_and(|m| m.is_dir()) {
        // create_dir_all follows a final symlink to an existing directory.
        // Remove a non-directory leaf before touching ownership or attributes.
        if let Ok(existing) = fs::symlink_metadata(destination) {
            if !existing.is_dir() {
                fs::remove_file(destination)?;
            }
        }
        fs::create_dir_all(destination)?;
        let src = src.unwrap();
        std::os::unix::fs::chown(destination, Some(src.uid()), Some(src.gid()))?;
        fs::set_permissions(destination, fs::Permissions::from_mode(src.mode()))?;
        let attrs = attributes(source)?;
        for key in xattr::list(destination)? {
            if !attrs.iter().any(|(k, _)| k == key.as_bytes()) {
                xattr::remove(destination, key)?;
            }
        }
        for (key, value) in attrs {
            xattr::set(destination, std::ffi::OsStr::from_bytes(&key), &value)?;
        }
        return Ok(());
    }
    if let Ok(m) = fs::symlink_metadata(destination) {
        if m.is_dir() {
            fs::remove_dir(destination)?;
        } else {
            fs::remove_file(destination)?;
        }
    }
    if src.is_some() {
        fs::create_dir_all(destination.parent().context("destination has no parent")?)?;
        command(
            "/usr/bin/cp",
            &[
                Path::new("-a"),
                Path::new("--reflink=always"),
                Path::new("--"),
                source,
                destination,
            ],
        )?;
    }
    Ok(())
}

fn attributes(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut attrs = Vec::new();
    for key in xattr::list(path)? {
        attrs.push((
            key.as_bytes().to_vec(),
            xattr::get(path, &key)?.context("xattr disappeared from frozen input")?,
        ));
    }
    attrs.sort();
    Ok(attrs)
}
