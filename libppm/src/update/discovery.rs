//! Ask Btrfs which paths changed; never replay the send commands themselves.
//! Complex directory moves and unavailable send privileges use the background
//! scan. Finalization has no scan fallback and leaves such work pending.
use super::{filesystem, validation};
use anyhow::{ensure, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) struct Delta {
    pub paths: BTreeSet<PathBuf>,
    pub scanned: usize,
}

pub(super) fn discover(
    before: &Path,
    after: &Path,
    budget: Duration,
    background: bool,
) -> Result<Option<Delta>> {
    if filesystem::identity(before)? == filesystem::identity(after)? {
        return Ok(Some(Delta {
            paths: BTreeSet::new(),
            scanned: 0,
        }));
    }
    let start = Instant::now();
    let limit = if background {
        64 * 1024 * 1024
    } else {
        4 * 1024 * 1024
    };
    let result = validation::stream_limited(before, after, budget, limit).and_then(|bytes| {
        let Some(bytes) = bytes else { return Ok(None) };
        let Some(paths) = paths(
            &bytes,
            before,
            if background { 262144 } else { 4096 },
            background,
            start + budget,
        )?
        else {
            return Ok(None);
        };
        let mut changed = BTreeSet::new();
        for path in paths {
            if start.elapsed() >= budget {
                return Ok(None);
            }
            let a = inspect(before, &path)
                .with_context(|| format!("inspecting previous {}", path.display()))?;
            let b = inspect(after, &path)
                .with_context(|| format!("inspecting latest {}", path.display()))?;
            // Directory timestamps change as a side effect of child writes.
            // Files are conservatively included even when only atime changed.
            if a != b || a.as_ref().is_some_and(|e| e.kind != "dir") {
                changed.insert(path);
            }
        }
        Ok(Some(Delta {
            paths: changed,
            scanned: 0,
        }))
    });
    if matches!(&result, Ok(Some(_))) || !background {
        return result;
    }
    let a = filesystem::manifest(before)?;
    let b = filesystem::manifest(after)?;
    let mut paths: BTreeSet<_> = a
        .keys()
        .chain(b.keys())
        .filter(|p| a.get(*p) != b.get(*p))
        .cloned()
        .collect();
    if filesystem::entry(before)? != filesystem::entry(after)? {
        paths.insert(PathBuf::new());
    }
    Ok(Some(Delta {
        paths,
        scanned: a.len() + b.len(),
    }))
}

/// Do not follow a renamed/replaced ancestor out of either frozen tree.
pub(super) fn inspect(root: &Path, path: &Path) -> Result<Option<filesystem::Entry>> {
    if path.as_os_str().is_empty() {
        return filesystem::inspect(root).map(Some);
    }
    let mut parent = root.to_owned();
    for part in path.parent().unwrap().components() {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(m) if m.is_dir() && !m.is_symlink() => {}
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    match filesystem::inspect(&root.join(path)) {
        Ok(e) => Ok(Some(e)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn paths(
    bytes: &[u8],
    before: &Path,
    limit: usize,
    background: bool,
    deadline: Instant,
) -> Result<Option<BTreeSet<PathBuf>>> {
    ensure!(
        bytes.len() >= 17 && &bytes[..13] == b"btrfs-stream\0",
        "invalid Btrfs stream header"
    );
    ensure!(
        u32::from_le_bytes(bytes[13..17].try_into()?) == 2,
        "unexpected send protocol"
    );
    let mut remaining = &bytes[17..];
    let mut started = false;
    let mut paths = BTreeSet::<PathBuf>::new();
    while !remaining.is_empty() {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        ensure!(remaining.len() >= 10, "truncated send command");
        let size = u32::from_le_bytes(remaining[..4].try_into()?) as usize;
        let command = u16::from_le_bytes(remaining[4..6].try_into()?);
        ensure!(
            size <= 65536 && remaining.len() >= size + 10,
            "invalid send command size"
        );
        let mut data = &remaining[10..10 + size];
        remaining = &remaining[10 + size..];
        if !started {
            ensure!(command == 2, "expected incremental snapshot");
            started = true;
            continue;
        }
        if command == 21 {
            ensure!(data.is_empty() && remaining.is_empty(), "invalid send end");
            return Ok(Some(paths));
        }
        ensure!(
            matches!(command, 3..=14 | 17..=20 | 22..=24),
            "unexpected no-data send operation"
        );
        let mut attrs = BTreeMap::new();
        while !data.is_empty() {
            ensure!(data.len() >= 4, "truncated send attribute");
            let key = u16::from_le_bytes(data[..2].try_into()?);
            let size = u16::from_le_bytes(data[2..4].try_into()?) as usize;
            ensure!(data.len() >= 4 + size, "invalid send attribute length");
            ensure!(
                attrs.insert(key, &data[4..4 + size]).is_none(),
                "duplicate send attribute"
            );
            data = &data[4 + size..];
        }
        let path = |key| -> Result<PathBuf> {
            let bytes = *attrs.get(&key).context("missing send path")?;
            ensure!(!bytes.contains(&0), "NUL in send path");
            if bytes.is_empty() || bytes == b"." {
                return Ok(PathBuf::new());
            }
            let p = PathBuf::from(OsStr::from_bytes(bytes));
            filesystem::relative(&p)?;
            Ok(p)
        };
        let source = path(15)?;
        paths.insert(source.clone());
        if command == 9 {
            let target = path(16)?;
            // Pacman replaces old DB directories using rename+unlink. Expand
            // just that directory in background, not every file in the root.
            // At shutdown even a subtree walk must stay in the background.
            if inspect(before, &source)?.is_some_and(|e| e.kind == "dir") {
                if !background {
                    return Ok(None);
                }
                filesystem::walk_tree(&before.join(&source), |p| {
                    ensure!(
                        paths.len() < limit && Instant::now() < deadline,
                        "directory move exceeds discovery budget"
                    );
                    paths.insert(p.strip_prefix(before)?.into());
                    Ok(())
                })?;
            }
            let moved: Vec<_> = paths
                .range(source.clone()..)
                .take_while(|p| p.starts_with(&source))
                .map(|p| {
                    let suffix = p.strip_prefix(&source).unwrap();
                    if suffix.as_os_str().is_empty() {
                        target.clone()
                    } else {
                        target.join(suffix)
                    }
                })
                .collect();
            paths.extend(moved);
            paths.insert(target);
        } else if command == 10 {
            // Hardlink aliases require topology-aware repair, not per-file copy.
            return Ok(None);
        }
        if paths.len() > limit {
            return Ok(None);
        }
    }
    anyhow::bail!("incomplete send stream")
}
