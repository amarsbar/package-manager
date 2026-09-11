use super::filesystem::relative;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Installed {
    pub record: PathBuf,
    pub version: String,
    pub files: BTreeSet<PathBuf>,
    pub backups: BTreeMap<PathBuf, String>,
    pub scriptlet: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Payload {
    pub source: PathBuf,
    pub md5: String,
    pub metadata: super::filesystem::Entry,
    pub missing_default: bool,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Package {
    pub name: String,
    pub version: String,
    pub files: BTreeMap<PathBuf, Payload>,
    pub backups: BTreeSet<PathBuf>,
    pub scriptlet: bool,
    pub old: Option<Installed>,
    pub unsupported: Option<String>,
}

fn sections(text: &str) -> BTreeMap<&str, Vec<&str>> {
    let mut out = BTreeMap::new();
    let mut key = "";
    for line in text.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            key = line.trim_matches('%');
        } else if !line.is_empty() {
            out.entry(key).or_insert_with(Vec::new).push(line);
        }
    }
    out
}

/// Only the default extraction/backup policy is modeled. Configuration includes
/// outside the watched pacman directory, globs and custom hook roots are opaque.
pub(super) fn default_options(root: &Path, db: &Path) -> Result<bool> {
    let first = PathBuf::from("etc/pacman.conf");
    if !root.join(&first).exists() {
        return Ok(true);
    } // caller-supplied fixture config
    let mut pending = vec![first];
    let mut seen = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if seen.len() > 128 {
            return Ok(false);
        }
        super::filesystem::ancestors(root, &path)?;
        let metadata = fs::symlink_metadata(root.join(&path))?;
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            return Ok(false);
        }
        for line in fs::read_to_string(root.join(&path))?.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "NoExtract" | "NoUpgrade" | "HookDir" => return Ok(false),
                "DBPath" if value.trim_matches('/') != db.to_str().unwrap_or_default() => {
                    return Ok(false)
                }
                "LogFile" if value != "/var/log/pacman.log" => return Ok(false),
                "RootDir" if value != "/" => return Ok(false),
                "Include" => {
                    if value.contains(['*', '?', '[']) {
                        return Ok(false);
                    }
                    let Ok(include) = Path::new(value).strip_prefix("/") else {
                        return Ok(false);
                    };
                    if !include.starts_with("etc/pacman.d") || relative(include).is_err() {
                        return Ok(false);
                    }
                    pending.push(include.into());
                }
                _ => {}
            }
        }
    }
    Ok(true)
}

pub(crate) fn installed(root: &Path, db: &Path) -> Result<BTreeMap<String, Installed>> {
    super::filesystem::ancestors(root, &db.join("local/entry"))?;
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(root.join(db).join("local"))? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(!metadata.is_symlink(), "symlink in installed database");
        if !metadata.is_dir() {
            continue;
        }
        for name in ["desc", "files"] {
            ensure!(
                fs::symlink_metadata(path.join(name))?.is_file(),
                "database records must be regular files"
            );
        }
        let text = fs::read_to_string(path.join("desc"))?;
        let desc = sections(&text);
        let name = desc
            .get("NAME")
            .and_then(|v| v.first())
            .context("missing package name")?;
        let version = desc
            .get("VERSION")
            .and_then(|v| v.first())
            .context("missing version")?;
        let text = fs::read_to_string(path.join("files"))?;
        let entries = sections(&text);
        let mut files = BTreeSet::new();
        let mut backups = BTreeMap::new();
        for p in entries.get("FILES").into_iter().flatten() {
            let p = PathBuf::from(p.trim_end_matches('/'));
            relative(&p)?;
            files.insert(p);
        }
        for b in entries.get("BACKUP").into_iter().flatten() {
            let (p, md5) = b.split_once('\t').context("invalid backup entry")?;
            relative(Path::new(p))?;
            backups.insert(p.into(), md5.into());
        }
        ensure!(
            out.insert(
                name.to_string(),
                Installed {
                    record: path.strip_prefix(root)?.into(),
                    version: version.to_string(),
                    files,
                    backups,
                    scriptlet: path.join("install").exists()
                }
            )
            .is_none(),
            "duplicate installed package"
        );
    }
    Ok(out)
}

impl Package {
    /// The successful native transaction is the payload authority. Archives are
    /// not required to describe dependencies or unchanged symlinks in a package.
    pub fn staged(
        name: &str,
        new: &Installed,
        old: Option<&Installed>,
        root: &Path,
    ) -> Result<Self> {
        let mut files = BTreeMap::new();
        let unsupported = None;
        for path in &new.files {
            let mut source = root.join(path);
            let Some(mut metadata) = super::discovery::inspect(root, path)? else {
                continue;
            };
            if metadata.kind == "dir" {
                continue;
            }
            if let Some(hash) = new.backups.get(path) {
                metadata = super::filesystem::entry(&source)?;
                if metadata.md5.as_ref() != Some(hash) {
                    let sidecar = PathBuf::from(format!("{}.pacnew", path.display()));
                    if let Some(entry) = super::filesystem::optional_entry(root, &sidecar)? {
                        if entry.md5.as_ref() == Some(hash) {
                            source = root.join(sidecar);
                            metadata = entry;
                        }
                    }
                }
            }
            files.insert(
                path.clone(),
                Payload {
                    source,
                    md5: new
                        .backups
                        .get(path)
                        .cloned()
                        .or_else(|| metadata.md5.clone())
                        .unwrap_or_default(),
                    missing_default: new
                        .backups
                        .get(path)
                        .is_some_and(|hash| metadata.md5.as_ref() != Some(hash)),
                    metadata,
                },
            );
        }
        Ok(Self {
            name: name.into(),
            version: new.version.clone(),
            files,
            backups: new.backups.keys().cloned().collect(),
            scriptlet: new.scriptlet,
            old: old.cloned(),
            unsupported,
        })
    }

    pub fn read(path: &Path, store: &Path, wanted: &BTreeSet<PathBuf>) -> Result<Self> {
        fs::create_dir(store)?;
        let mut file = fs::File::open(path)?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        file.seek(SeekFrom::Start(0))?;
        let reader: Box<dyn Read> = if magic == [0x28, 0xb5, 0x2f, 0xfd] {
            Box::new(zstd::Decoder::new(file)?)
        } else {
            Box::new(file)
        };
        let mut tar = tar::Archive::new(reader);
        tar.set_preserve_ownerships(true);
        tar.set_preserve_permissions(true);
        tar.set_preserve_mtime(true);
        let mut info = String::new();
        let mut files = BTreeMap::new();
        let mut scriptlet = false;
        let mut unsupported = None;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            relative(&path)?;
            if path == Path::new(".PKGINFO") {
                entry.read_to_string(&mut info)?;
                continue;
            }
            if path == Path::new(".INSTALL") {
                scriptlet = true;
                continue;
            }
            if path == Path::new(".MTREE") || path == Path::new(".BUILDINFO") {
                continue;
            }
            if !wanted.contains(&path) {
                continue;
            }
            if entry.header().entry_type().is_dir() {
                continue;
            }
            if !entry.header().entry_type().is_file() || entry.pax_extensions()?.is_some() {
                unsupported = Some("non-regular archive entry or extended metadata".into());
                continue;
            }
            let target = store.join(files.len().to_string());
            entry.unpack(&target)?;
            let metadata = super::filesystem::entry(&target)?;
            let md5 = metadata
                .md5
                .clone()
                .context("cached payload is not a regular file")?;
            ensure!(
                files
                    .insert(
                        path,
                        Payload {
                            source: target,
                            md5,
                            metadata,
                            missing_default: false
                        }
                    )
                    .is_none(),
                "duplicate archive path"
            );
        }
        let fields: Vec<_> = info.lines().filter_map(|l| l.split_once(" = ")).collect();
        let field = |name| {
            fields
                .iter()
                .find_map(|(k, v)| (*k == name).then_some(*v))
                .with_context(|| format!("missing {name}"))
        };
        let name = field("pkgname")?.into();
        let version = field("pkgver")?.into();
        let mut backups = BTreeSet::new();
        for (key, path) in fields {
            if key == "backup" {
                relative(Path::new(path))?;
                backups.insert(path.into());
            }
        }
        if files.is_empty() {
            unsupported = Some("package has no regular-file optimization".into());
        }
        Ok(Self {
            name,
            version,
            files,
            backups,
            scriptlet,
            old: None,
            unsupported,
        })
    }
}
