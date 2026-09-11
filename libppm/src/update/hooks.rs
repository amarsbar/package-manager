//! Reuse rules describe reads, not merely observed writes. An unrecognized
//! computation is opaque: a divergent root needs native background execution.
use super::filesystem;
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    path::{Path, PathBuf},
};

/// A conservative superset of hooks that could have run. Logs are optional:
/// /var/log is normally shared and absent from the sealed root snapshot.
/// Operations are deliberately over-approximated; an uncertain match reruns
/// native pacman instead of reusing opaque output.
pub(super) fn possible(
    root: &Path,
    packages: &BTreeSet<String>,
    paths: &BTreeSet<PathBuf>,
) -> Result<BTreeSet<String>> {
    let mut hooks = BTreeMap::new();
    for directory in ["usr/share/libalpm/hooks", "etc/pacman.d/hooks"] {
        filesystem::ancestors(root, &Path::new(directory).join("entry"))?;
        let entries = match fs::read_dir(root.join(directory)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 hook name"))?;
            if name.ends_with(".hook") {
                hooks.insert(name, entry.path());
            }
        }
    }
    let file_targets: Vec<_> = paths
        .iter()
        .flat_map(|p| {
            let p = p.to_string_lossy().into_owned();
            [p.clone(), format!("{p}/")]
        })
        .collect();
    let package_targets: Vec<_> = packages.iter().cloned().collect();
    let mut result = BTreeSet::new();
    for (name, path) in hooks {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_symlink() && fs::read_link(&path)? == Path::new("/dev/null") {
            continue;
        }
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            result.insert(name);
            continue;
        }
        let text = fs::read_to_string(path)?;
        if could_match(&text, &package_targets, &file_targets).unwrap_or(true) {
            result.insert(name);
        }
    }
    Ok(result)
}

fn could_match(text: &str, packages: &[String], paths: &[String]) -> Option<bool> {
    let mut triggers: Vec<(Option<&str>, Vec<&str>)> = Vec::new();
    let mut section = "";
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = line;
            if section == "[Trigger]" {
                triggers.push((None, Vec::new()));
            } else if section != "[Action]" {
                return None;
            }
            continue;
        }
        if section != "[Trigger]" {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let trigger = triggers.last_mut()?;
        match key.trim() {
            "Type" if trigger.0.is_none() => trigger.0 = Some(value.trim()),
            "Target" => {
                // Do not guess at quoting, escaping or comment syntax. A
                // syntax difference must over-match hooks, never hide one.
                let value = value.trim();
                if value.contains(['\'', '"', '\\', '#']) || value.chars().any(char::is_whitespace)
                {
                    return None;
                }
                trigger.1.push(value);
            }
            "Operation" if matches!(value.trim(), "Install" | "Upgrade" | "Remove") => {}
            _ => return None,
        }
    }
    if triggers.is_empty() {
        return None;
    }
    let mut any = false;
    for (kind, patterns) in triggers {
        let values = match kind? {
            "Package" => packages,
            "Path" | "File" => paths,
            _ => return None,
        };
        if patterns.is_empty() {
            return None;
        }
        for value in values {
            let value = CString::new(value.as_str()).ok()?;
            let mut matched = false;
            for pattern in &patterns {
                let (positive, pattern) = match pattern.strip_prefix('!') {
                    Some(p) => (false, p),
                    None => (true, *pattern),
                };
                let pattern = CString::new(pattern).ok()?;
                if unsafe { libc::fnmatch(pattern.as_ptr(), value.as_ptr(), 0) } == 0 {
                    matched = positive;
                }
            }
            any |= matched;
        }
    }
    Some(any)
}

/// Audited Arch systemd-hook implementation, including the `update` branch
/// which only touches /usr for ConditionNeedsUpdate. Changing the script bytes
/// invalidates this rule; matching its filename alone is not enough.
const SYSTEMD_HOOK: &str = "90e00dd01359565be85a8cdd93b424b4c1c89563300d16eaf0e7eedc349bf9fc";

pub(super) fn update_marker(root: &Path, name: &str) -> Result<bool> {
    filesystem::relative(Path::new(name))?;
    if Path::new(name).components().count() != 1 {
        return Ok(false);
    }
    let mut hook = None;
    for directory in ["usr/share/libalpm/hooks", "etc/pacman.d/hooks"] {
        let relative = Path::new(directory).join(name);
        filesystem::ancestors(root, &relative)?;
        match fs::symlink_metadata(root.join(&relative)) {
            Ok(m) if m.is_file() => hook = Some(fs::read_to_string(root.join(relative))?),
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some(hook) = hook else { return Ok(false) };
    let values = |key: &str| -> Vec<&str> {
        hook.lines()
            .filter_map(|line| {
                let (k, v) = line.split_once('=')?;
                (k.trim() == key).then_some(v.trim())
            })
            .collect()
    };
    if values("Exec") != ["/usr/share/libalpm/scripts/systemd-hook update"]
        || values("When") != ["PostTransaction"]
    {
        return Ok(false);
    }
    Ok(
        filesystem::optional_entry(root, Path::new("usr/share/libalpm/scripts/systemd-hook"))?
            .and_then(|e| e.sha256)
            .as_deref()
            == Some(SYSTEMD_HOOK),
    )
}

/// Conservative userspace/runtime/config closure for the supported commands.
/// It intentionally includes more than their usual direct file inputs.
pub(super) fn runtime_input(path: &Path) -> bool {
    path.as_os_str().is_empty()
        || ["usr", "etc", "opt", "boot", "bin", "sbin", "lib", "lib64"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
        || path.starts_with("var/cache/ldconfig")
}
