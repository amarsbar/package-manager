//! Reconcile completed native-pacman transactions with a later, frozen root.
//! Expensive work runs in the background; this module never selects a boot entry.

mod candidate;
mod discovery;
mod filesystem;
mod hooks;
mod package;
mod validation;

pub use candidate::{Candidate, Validation};

use anyhow::{ensure, Context, Result};
use filesystem::{copy_entry, frozen, identity, relative, Entry, SnapshotIdentity};
use package::{installed, Package};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

/// A refusal is not permission to activate the old staged output. Re-stage in
/// the background or leave the current system selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotReady {
    pub reason: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum RebaseOutcome {
    Candidate(Box<Candidate>),
    NotReady(NotReady),
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RebaseStats {
    pub native_restage: bool,
    pub restage_reason: Option<NotReady>,
    pub scanned_paths: usize,
    pub discovered_paths: usize,
    pub carried_paths: usize,
    pub repaired_files: usize,
    pub total_ms: f64,
}

/// A completed native transaction prepared for staged-payload reuse.
///
/// `base` and `staged` are read-only snapshots before/after a successful native
/// transaction using default file-handling options, the supplied DB path and
/// the default log location (which may be outside the snapshot). Optional archives supply backup defaults which staging
/// may not contain; normal payload comes from staging. All changed packages,
/// including dependencies and reinstalls, are derived from the changed DB records
/// and, when available, the transaction log.
///
/// The caller attests that staging is exactly that successful native output,
/// and owns transaction execution, isolation and signature verification. This
/// is NOT a second package-integrity verifier. Nothing
/// writes to either input or selects a boot entry. Candidate readiness is a
/// separate, final check; a successful background rebase does not authorize boot.
///
/// Inputs must be private, quiescent snapshot trees with no nested mounts or
/// subvolumes (empty Btrfs snapshot placeholders are allowed). The caller must
/// retain them for the lifetime of this preparation.
#[derive(Serialize, Deserialize)]
pub struct PreparedUpdate {
    schema: u32,
    base: PathBuf,
    staged: PathBuf,
    base_identity: SnapshotIdentity,
    staged_identity: SnapshotIdentity,
    store: PathBuf,
    db: PathBuf,
    packages: Vec<Package>,
    owned: BTreeSet<PathBuf>,
    refusal: Option<NotReady>,
    runtime_inputs: bool,
    log_append: Vec<u8>,
}

impl PreparedUpdate {
    /// Background operation: reuse supported work, otherwise run the original
    /// update through the caller's native transaction executor on a NEW private
    /// writable snapshot of `latest`. Never call this during final activation.
    ///
    /// `apply` must perform the intended transaction, including dependency and
    /// signature checks, hooks and scriptlets, exclusively in the supplied root.
    /// It must not expose writable live shared mounts, leave subprocesses or
    /// mounts running, or return success for a failed transaction. The executor
    /// is supplied because this module consumes completed transactions; it does
    /// not own repository/AUR resolution or guess a replacement pacman command.
    pub fn refresh(
        &self,
        latest: &Path,
        destination: &Path,
        apply: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Candidate> {
        let start = Instant::now();
        let latest = frozen(latest)?;
        let latest_identity = identity(&latest)?;
        if identity(&self.base)? == self.base_identity
            && identity(&self.staged)? == self.staged_identity
        {
            // A completed native transaction needs no modeled hook/scriptlet
            // dependencies if its entire input tree is still unchanged. This
            // check runs in the background, never as a synchronous fallback.
            let unchanged = latest_identity == self.base_identity
                || matches!(
                    validation::compare(&self.base, &latest, std::time::Duration::from_secs(5)),
                    Ok(Validation::Ready)
                );
            if unchanged {
                filesystem::publish(&self.staged, destination)?;
                ensure!(
                    identity(&self.base)? == self.base_identity
                        && identity(&self.staged)? == self.staged_identity,
                    "completed transaction changed during reuse"
                );
                let mut candidate = Candidate::new(
                    destination,
                    &latest,
                    latest_identity,
                    RebaseStats {
                        total_ms: start.elapsed().as_secs_f64() * 1000.,
                        ..Default::default()
                    },
                )?;
                candidate.preparation = Some((self.store.clone(), self.staged_identity.clone()));
                return Ok(candidate);
            }
        }
        let mut candidate = match self.rebase(&latest, destination)? {
            RebaseOutcome::Candidate(candidate) => *candidate,
            RebaseOutcome::NotReady(reason) => {
                let mut candidate = restage(&latest, destination, apply)?;
                candidate.stats.restage_reason = Some(reason);
                candidate
            }
        };
        candidate.stats.total_ms = start.elapsed().as_secs_f64() * 1000.;
        Ok(candidate)
    }

    pub fn prepare(
        base: &Path,
        staged: &Path,
        archives: &[PathBuf],
        db: &Path,
        store: &Path,
    ) -> Result<Self> {
        let base = frozen(base)?;
        let staged = frozen(staged)?;
        let base_identity = identity(&base)?;
        let staged_identity = identity(&staged)?;
        relative(db)?;
        ensure!(!store.exists(), "preparation destination already exists");
        filesystem::check_tree(&base)?;
        filesystem::check_tree(&staged)?;
        let staging_delta =
            discovery::discover(&base, &staged, std::time::Duration::from_secs(30), true)?
                .context("staging discovery did not complete")?;
        let root_changed = filesystem::entry(&base)? != filesystem::entry(&staged)?;
        let old = installed(&base, db)?;
        let new = installed(&staged, db)?;
        let mut owned: BTreeSet<_> = old.values().flat_map(|p| p.files.iter().cloned()).collect();
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(store)?;
        let store = fs::canonicalize(store)?;
        ensure!(
            !store.starts_with(&base) && !store.starts_with(&staged),
            "preparation overlaps an input"
        );
        let mut packages = Vec::new();
        let mut refusal = if root_changed {
            Some(NotReady {
                reason: "transaction needs native background re-staging".into(),
                path: None,
            })
        } else {
            None
        };
        if !package::default_options(&base, db)? || !package::default_options(&staged, db)? {
            refusal = Some(NotReady { reason: "non-default pacman file handling or configuration needs native background execution".into(), path: Some("etc/pacman.conf".into()) });
        }
        let old_log = read_log(&base)?;
        let staged_log = read_log(&staged)?;
        let log_tail = staged_log
            .strip_prefix(old_log.as_slice())
            .unwrap_or_default();
        let log_text = String::from_utf8_lossy(log_tail);
        let mut names = BTreeSet::new();
        for line in log_text.lines() {
            if let Some((_, action)) = line.split_once("[ALPM] ") {
                let mut words = action.split_whitespace();
                if matches!(
                    words.next(),
                    Some("installed" | "upgraded" | "downgraded" | "reinstalled" | "removed")
                ) {
                    if let Some(name) = words.next() {
                        names.insert(name.to_string());
                    }
                }
            }
        }
        if staging_delta.scanned != 0 && names.is_empty() {
            // A content scan can miss a same-version reinstall whose DB bytes
            // and timestamps are unchanged. Kernel inode events or the ALPM
            // log are needed to establish the complete set of hook triggers.
            refusal = Some(NotReady {
                reason: "transaction discovery needs Btrfs events or its ALPM log; use native background execution".into(),
                path: None,
            });
        }
        for name in old.keys() {
            if !new.contains_key(name) {
                refusal = Some(NotReady {
                    reason: format!(
                        "removed or undeclared package needs native re-staging: {name}"
                    ),
                    path: None,
                });
            }
        }
        let records: BTreeSet<_> = staging_delta
            .paths
            .iter()
            .filter_map(|path| {
                let suffix = path.strip_prefix(db.join("local")).ok()?;
                Some(db.join("local").join(suffix.components().next()?))
            })
            .collect();
        for (name, next) in &new {
            if old.get(name) != Some(next) || names.contains(name) || records.contains(&next.record)
            {
                let p = Package::staged(name, next, old.get(name), &staged)?;
                if p.scriptlet
                    || p.old.as_ref().is_some_and(|p| p.scriptlet)
                    || p.unsupported.is_some()
                {
                    refusal = Some(NotReady {
                        reason: p
                            .unsupported
                            .clone()
                            .unwrap_or_else(|| "transaction contains a scriptlet".into()),
                        path: None,
                    });
                }
                packages.push(p);
            }
        }
        // Optional package archives provide backup defaults only. The normal
        // payload (including symlinks and dependencies) always comes from the
        // successful staged tree, never a second bulk extraction.
        let wanted: BTreeSet<_> = packages
            .iter()
            .flat_map(|p| {
                p.files
                    .iter()
                    .filter(|(_, f)| f.missing_default)
                    .map(|(p, _)| p.clone())
            })
            .collect();
        for (index, archive) in archives.iter().enumerate().filter(|_| !wanted.is_empty()) {
            let cached = Package::read(archive, &store.join(format!("package-{index}")), &wanted)?;
            let package = packages
                .iter_mut()
                .find(|p| p.name == cached.name)
                .context("archive is not in the completed transaction")?;
            ensure!(
                package.version == cached.version,
                "archive and staged database versions disagree"
            );
            for (path, file) in cached.files {
                if !package.backups.contains(&path) {
                    continue;
                }
                let default = package
                    .files
                    .get_mut(&path)
                    .context("backup absent from staged package")?;
                if !default.missing_default {
                    continue;
                }
                ensure!(
                    file.md5 == default.md5,
                    "cached archive disagrees with the staged backup digest"
                );
                *default = file;
            }
        }
        let mut runtime_inputs = fs::symlink_metadata(staged.join("usr/bin/ldconfig")).is_ok();
        let mut hook_names = BTreeSet::new();
        for line in log_text.lines() {
            if let Some((_, action)) = line.split_once("[ALPM] running '") {
                let name = action.split('\'').next().context("invalid hook log")?;
                hook_names.insert(name.to_string());
            }
        }
        let transaction_names = packages.iter().map(|p| p.name.clone()).collect();
        let transaction_paths = packages
            .iter()
            .flat_map(|p| {
                new[&p.name]
                    .files
                    .iter()
                    .chain(p.old.iter().flat_map(|p| p.files.iter()))
                    .cloned()
            })
            .collect();
        for root in [&base, &staged] {
            hook_names.extend(hooks::possible(
                root,
                &transaction_names,
                &transaction_paths,
            )?);
        }
        for name in hook_names {
            if hooks::update_marker(&base, &name)? && hooks::update_marker(&staged, &name)? {
                runtime_inputs = true;
            } else {
                refusal = Some(NotReady {
                    reason: format!("hook has no audited input rule: {name}"),
                    path: None,
                });
            }
        }
        let backup_paths: BTreeSet<_> = packages
            .iter()
            .flat_map(|p| {
                p.backups
                    .iter()
                    .chain(p.old.iter().flat_map(|p| p.backups.keys()))
            })
            .flat_map(|p| [p.clone(), sidecar(p, ".pacnew"), sidecar(p, ".pacsave")])
            .collect();
        let backup_entries = |root: &Path| -> Result<BTreeMap<PathBuf, Entry>> {
            let mut entries = BTreeMap::new();
            for path in &backup_paths {
                if let Some(entry) = filesystem::optional_entry(root, path)? {
                    entries.insert(path.clone(), entry);
                }
            }
            Ok(entries)
        };
        let before = backup_entries(&base)?;
        let after = backup_entries(&staged)?;
        // Check backup decisions, where native staging may preserve user bytes
        // instead of the package default. The caller attests to the completed
        // native transaction; ordinary staged payload is not re-certified here.
        for package in packages.iter().filter(|_| refusal.is_none()) {
            for (path, file) in &package.files {
                if !package.backups.contains(path) {
                    continue;
                }
                let old = package.old.as_ref().and_then(|p| p.backups.get(path));
                let current = before.get(path).and_then(|e| e.md5.as_ref());
                let target = if package.backups.contains(path) {
                    backup_target(path, old, current, &file.md5)?
                } else {
                    Some(path.clone())
                };
                if target.as_ref() != Some(path) {
                    ensure!(
                        after.get(path) == before.get(path),
                        "staged backup preservation disagrees with default pacman: {}",
                        path.display()
                    );
                }
                if let Some(target) = target {
                    ensure!(
                        after.get(&target) == Some(&file.metadata),
                        "staged payload or metadata disagrees with archive: {}",
                        target.display()
                    );
                }
            }
        }
        let mut effects = BTreeSet::new();
        for p in &packages {
            effects.extend(p.files.keys().cloned());
            if let Some(old) = &p.old {
                effects.extend(old.files.iter().cloned());
            }
            for path in p
                .backups
                .iter()
                .chain(p.old.iter().flat_map(|p| p.backups.keys()))
            {
                for suffix in [".pacnew", ".pacsave"] {
                    effects.insert(PathBuf::from(format!(
                        "{}{suffix}",
                        path.to_str().context("non-UTF8 backup path")?
                    )));
                }
            }
        }
        for path in self_and_parents(&effects) {
            effects.insert(path);
        }
        owned.retain(|path| effects.contains(path));
        for path in &staging_delta.paths {
            if path.as_os_str().is_empty() {
                continue;
            }
            if path.starts_with(db)
                || effects.contains(path)
                || path == Path::new("var/log/pacman.log")
                || path == Path::new("var/log")
                || (runtime_inputs
                    && (path == Path::new("etc/ld.so.cache")
                        || path.starts_with("var/cache/ldconfig")))
            {
                continue;
            }
            refusal = Some(NotReady {
                reason: "staged transaction has an unmodeled filesystem effect".into(),
                path: Some(path.clone()),
            });
            break;
        }
        let old_log = read_log(&base)?;
        let new_log = read_log(&staged)?;
        let log_append = if new_log.starts_with(&old_log) {
            new_log[old_log.len()..].to_vec()
        } else {
            refusal = Some(NotReady {
                reason: "staged pacman log was not append-only".into(),
                path: None,
            });
            Vec::new()
        };
        ensure!(
            identity(&base)? == base_identity && identity(&staged)? == staged_identity,
            "snapshot changed during preparation"
        );
        let update = Self {
            schema: 1,
            base,
            staged,
            base_identity,
            staged_identity,
            store,
            db: db.into(),
            packages,
            owned,
            refusal,
            runtime_inputs,
            log_append,
        };
        for package in &update.packages {
            for file in package.files.values() {
                if file.source.starts_with(&update.store) {
                    fs::File::open(&file.source)?.sync_all()?;
                }
            }
            if let Some(file) = package.files.values().next() {
                fs::File::open(
                    file.source
                        .parent()
                        .context("cached payload has no parent")?,
                )?
                .sync_all()?;
            }
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(update.store.join("plan.partial"))?;
        serde_json::to_writer(&mut file, &update)?;
        file.sync_all()?;
        fs::rename(
            update.store.join("plan.partial"),
            update.store.join("plan.json"),
        )?;
        fs::File::open(&update.store)?.sync_all()?;
        Ok(update)
    }

    /// Load a complete preparation. Partial, relocated, or unknown-version
    /// preparations are never treated as an update ready to rebase.
    pub fn open(store: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let store = fs::canonicalize(store)?;
        let metadata = fs::metadata(&store)?;
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "preparation must be private and owned by the updater's effective user"
        );
        let path = store.join("plan.json");
        let update: Self = serde_json::from_reader(std::io::BufReader::new(fs::File::open(path)?))?;
        ensure!(
            update.schema == 1,
            "unsupported preparation version; re-stage"
        );
        ensure!(update.store == store, "preparation was relocated; re-stage");
        relative(&update.db)?;
        for path in &update.owned {
            relative(path)?;
        }
        for package in &update.packages {
            for (path, payload) in &package.files {
                relative(path)?;
                if let Ok(relative_path) = payload.source.strip_prefix(&update.staged) {
                    relative(relative_path)?;
                    filesystem::ancestors(&update.staged, relative_path)?;
                } else {
                    ensure!(
                        fs::canonicalize(&payload.source)?.starts_with(&store),
                        "payload is outside preparation"
                    );
                }
            }
            for path in &package.backups {
                relative(path)?;
            }
            if let Some(old) = &package.old {
                relative(&old.record)?;
                for path in old.files.iter().chain(old.backups.keys()) {
                    relative(path)?;
                }
            }
        }
        frozen(&update.base)?;
        frozen(&update.staged)?;
        Ok(update)
    }

    pub fn rebase(&self, latest: &Path, destination: &Path) -> Result<RebaseOutcome> {
        self.rebase_from(&self.staged, &self.base, latest, destination, None)
    }

    /// Carry a bounded final delta into an already prepared candidate. No scan
    /// fallback, archive extraction, hook execution or native pacman is allowed
    /// here. Root writers must have stopped and remain stopped until selection.
    /// A refusal or expired budget means boot the existing root and refresh later.
    pub fn finish(
        &self,
        candidate: &Candidate,
        final_root: &Path,
        destination: &Path,
        budget: std::time::Duration,
    ) -> Result<RebaseOutcome> {
        let deadline = Instant::now() + budget;
        candidate.check_identities()?;
        ensure!(
            candidate.preparation.as_ref()
                == Some(&(self.store.clone(), self.staged_identity.clone())),
            "candidate belongs to a different preparation or native restage"
        );
        let result = self.rebase_from(
            candidate.path(),
            candidate.reference(),
            final_root,
            destination,
            Some(deadline),
        )?;
        candidate.check_identities()?;
        if Instant::now() >= deadline {
            return Ok(not_ready("final reconciliation budget expired", None));
        }
        Ok(result)
    }

    fn rebase_from(
        &self,
        image: &Path,
        reference: &Path,
        latest: &Path,
        destination: &Path,
        deadline: Option<Instant>,
    ) -> Result<RebaseOutcome> {
        let start = Instant::now();
        frozen(&self.base)?;
        frozen(&self.staged)?;
        let latest = frozen(latest)?;
        let latest_identity = identity(&latest)?;
        ensure!(
            !destination.exists(),
            "candidate destination already exists"
        );
        if identity(&self.base)? != self.base_identity
            || identity(&self.staged)? != self.staged_identity
        {
            return Ok(RebaseOutcome::NotReady(NotReady {
                reason: "a preparation input changed; re-stage".into(),
                path: None,
            }));
        }
        if let Some(reason) = &self.refusal {
            return Ok(RebaseOutcome::NotReady(reason.clone()));
        }
        let budget = deadline
            .map(|end| end.saturating_duration_since(Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(5));
        if budget.is_zero() {
            return Ok(not_ready("final reconciliation budget expired", None));
        }
        let Some(delta) = discovery::discover(reference, &latest, budget, deadline.is_none())?
        else {
            return Ok(not_ready("delta needs background discovery", None));
        };
        let mut changed = delta.paths;
        if self.runtime_inputs {
            if let Some(path) = changed.iter().find(|path| hooks::runtime_input(path)) {
                return Ok(not_ready("a supported hook or ldconfig input changed; rerun native pacman in the background", Some(path.clone())));
            }
        }
        changed.remove(&PathBuf::new()); // Root metadata is carried separately.
        let backups: BTreeSet<_> = self
            .packages
            .iter()
            .flat_map(|p| {
                p.backups
                    .iter()
                    .chain(p.old.iter().flat_map(|p| p.backups.keys()))
            })
            .cloned()
            .collect();
        let touched_backups: BTreeSet<_> = backups
            .into_iter()
            .filter(|p| {
                changed.iter().any(|c| {
                    p.starts_with(c) || c == &sidecar(p, ".pacnew") || c == &sidecar(p, ".pacsave")
                })
            })
            .collect();
        if deadline.is_some() {
            for path in &touched_backups {
                for root in [reference, latest.as_path()] {
                    if discovery::inspect(root, path)?.is_some_and(|e| e.size > 1024 * 1024) {
                        return Ok(not_ready(
                            "large backup comparison needs the background",
                            Some(path.clone()),
                        ));
                    }
                }
            }
        }
        let mut relevant = self_and_parents(&changed);
        for path in &touched_backups {
            for suffix in ["", ".pacnew", ".pacsave"] {
                relevant.insert(sidecar(path, suffix));
            }
        }
        let read = |root: &Path| -> Result<BTreeMap<PathBuf, Entry>> {
            let mut result = BTreeMap::new();
            for path in &relevant {
                if deadline.is_some_and(|end| Instant::now() >= end) {
                    anyhow::bail!("final reconciliation budget expired");
                }
                if let Some(mut entry) = discovery::inspect(root, path)? {
                    if touched_backups.contains(path) && entry.kind == "file" {
                        entry = filesystem::entry(&root.join(path))?;
                    }
                    result.insert(path.clone(), entry);
                }
            }
            Ok(result)
        };
        let before = read(reference)?;
        let current = read(&latest)?;
        let mut stats = RebaseStats {
            scanned_paths: delta.scanned,
            discovered_paths: changed.len(),
            ..Default::default()
        };
        let refuse = |reason: &str, path: &Path| {
            Ok(RebaseOutcome::NotReady(NotReady {
                reason: reason.into(),
                path: Some(path.into()),
            }))
        };
        for (path, entry) in before.iter().chain(&current) {
            if entry.kind == "other" {
                return refuse("special filesystem nodes require native re-staging", path);
            }
            if entry.write_flags != 0 {
                return refuse(
                    "immutable or append-only files require native re-staging",
                    path,
                );
            }
        }
        let log = Path::new("var/log/pacman.log");
        filesystem::ancestors(&latest, log)?;
        if current
            .get(log)
            .is_some_and(|e| e.kind != "file" || e.links > 1)
        {
            return refuse("pacman log is not an ordinary file", log);
        }
        for path in &changed {
            if path.starts_with(&self.db) {
                return refuse(
                    "installed database changed; re-stage against the new package state",
                    path,
                );
            }
            if path == Path::new("etc/pacman.conf")
                || path.starts_with("etc/pacman.d")
                || path.starts_with("usr/share/libalpm")
            {
                return refuse("pacman configuration or hooks changed", path);
            }
            for entries in [&before, &current] {
                if entries
                    .get(path)
                    .is_some_and(|e| e.links > 1 && e.kind == "file")
                {
                    return refuse("changed hardlink requires native re-staging", path);
                }
            }
        }
        let mut actions = Vec::new();
        let mut removals = Vec::new();
        let mut restore = BTreeSet::new();
        let transaction_files: BTreeSet<_> = self
            .packages
            .iter()
            .flat_map(|p| p.files.keys().cloned())
            .collect();
        let root_metadata = filesystem::entry(&latest)?;
        if root_metadata.write_flags != 0 {
            return refuse("root write flags require native re-staging", Path::new("."));
        }
        for path in &transaction_files {
            for parent in path.ancestors().skip(1) {
                let entry = if parent.as_os_str().is_empty() {
                    Some(&root_metadata)
                } else {
                    current.get(parent)
                };
                if entry.is_some_and(|e| {
                    e.xattrs
                        .iter()
                        .any(|(key, _)| key == b"system.posix_acl_default")
                }) {
                    return refuse("default ACL inheritance requires native re-staging", parent);
                }
            }
        }
        for package in &self.packages {
            if let Some(old) = &package.old {
                for path in old.files.difference(&transaction_files) {
                    if !changed.contains(path) && !touched_backups.contains(path) {
                        continue;
                    }
                    if before.get(path).is_some_and(|e| e.kind == "dir") {
                        continue;
                    }
                    filesystem::ancestors(&latest, path)?;
                    let entry = current.get(path);
                    if entry.is_some_and(|e| e.kind != "file" || e.links > 1) {
                        return refuse("removed target is not an ordinary file", path);
                    }
                    let save = old.backups.get(path).is_some_and(|old| {
                        entry
                            .and_then(|e| e.md5.as_ref())
                            .is_some_and(|now| now != old)
                    });
                    let saved = PathBuf::from(format!(
                        "{}.pacsave",
                        path.to_str().context("non-UTF8 package path")?
                    ));
                    if save && current.contains_key(&saved) {
                        return refuse("existing pacsave needs native backup rotation", &saved);
                    }
                    removals.push((path.clone(), save.then_some(saved)));
                    if old.backups.contains_key(path) {
                        restore.insert(path.clone());
                    }
                }
            }
            for (path, file) in &package.files {
                if !changed.contains(path) && !touched_backups.contains(path) {
                    continue;
                }
                if package.backups.contains(path) {
                    restore.insert(path.clone());
                }
                filesystem::ancestors(&latest, path)?;
                if current.get(path).is_some_and(|e| {
                    e.kind != file.metadata.kind || !matches!(e.kind.as_str(), "file" | "link")
                }) {
                    return refuse("package target is not a regular file", path);
                }
                if file.metadata.links > 1 || current.get(path).is_some_and(|e| e.links > 1) {
                    return refuse("package target has hardlink aliases", path);
                }
                if current.contains_key(path) && !self.owned.contains(path) {
                    return refuse("package target exists but is not owned", path);
                }
                let old_hash = package.old.as_ref().and_then(|p| p.backups.get(path));
                let current_hash = current.get(path).and_then(|e| e.md5.as_ref());
                let target = if package.backups.contains(path) {
                    match backup_target(path, old_hash, current_hash, &file.md5)? {
                        Some(target) => target,
                        None => continue,
                    }
                } else {
                    path.clone()
                };
                // The staged payload is already correct unless a late edit or a
                // backup decision can change this target. No bulk payload copy.
                if package.backups.contains(path) || changed.contains(path) {
                    filesystem::ancestors(&latest, &target)?;
                    if current
                        .get(&target)
                        .is_some_and(|e| e.kind != "file" || e.links > 1)
                    {
                        return refuse("backup destination is not an ordinary file", &target);
                    }
                    actions.push((file, target));
                }
            }
        }
        // Only repair payloads are read here. The bulk installed payload stays
        // in the identity-pinned staged snapshot and is never re-extracted.
        for (file, _) in &actions {
            if file.missing_default {
                return Ok(not_ready("required package default is not in staging; prepare with the cached archive or restage", None));
            }
            if file.source.starts_with(&self.staged) {
                continue;
            }
            if deadline.is_some() && file.metadata.size > 1024 * 1024 {
                return Ok(not_ready(
                    "large cached backup repair needs the background",
                    None,
                ));
            }
            ensure!(
                filesystem::entry(&file.source)? == file.metadata,
                "cached repair metadata is damaged: {}",
                file.source.display()
            );
        }
        let mut restore_paths = BTreeSet::new();
        for path in restore {
            for suffix in ["", ".pacnew", ".pacsave"] {
                let p = PathBuf::from(format!(
                    "{}{suffix}",
                    path.to_str().context("non-UTF8 package path")?
                ));
                filesystem::ancestors(&latest, &p)?;
                if !suffix.is_empty() && (self.owned.contains(&p) || transaction_files.contains(&p))
                {
                    return refuse("backup sidecar collides with a package file", &p);
                }
                if current
                    .get(&p)
                    .is_some_and(|e| e.kind != "file" || e.links > 1)
                {
                    return refuse("backup input is not an ordinary file", &p);
                }
                restore_paths.insert(p);
            }
        }
        // All refusal checks precede candidate creation. Inputs remain read-only;
        // only this newly created subvolume may be changed below.
        let final_destination = destination;
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Ok(not_ready("final reconciliation budget expired", None));
        }
        let work = filesystem::WorkingRoot::new(image, destination)?;
        let destination = work.path();
        copy_entry(&latest, destination)?;
        let carry_order: Vec<_> = changed
            .iter()
            .rev()
            .filter(|p| !current.contains_key(*p))
            .chain(changed.iter().filter(|p| current.contains_key(*p)))
            .collect();
        for path in carry_order {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(not_ready("final reconciliation budget expired", None));
            }
            filesystem::ancestors(destination, path)?;
            // A transaction may have removed a directory that now contains
            // unrelated late user data. Restore its live metadata as well.
            let parents: Vec<_> = path
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            if current.contains_key(path) {
                for parent in parents.into_iter().rev() {
                    if current.get(parent).is_some_and(|e| e.kind == "dir")
                        && !destination.join(parent).exists()
                    {
                        copy_entry(&latest.join(parent), &destination.join(parent))?;
                    }
                }
            }
            copy_entry(&latest.join(path), &destination.join(path))?;
            stats.carried_paths += 1;
        }
        for path in restore_paths {
            filesystem::ancestors(destination, &path)?;
            copy_entry(&latest.join(&path), &destination.join(path))?;
        }
        for (file, path) in actions {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(not_ready("final reconciliation budget expired", None));
            }
            filesystem::ancestors(destination, &path)?;
            copy_entry(&file.source, &destination.join(path))?;
            stats.repaired_files += 1;
        }
        for (path, saved) in removals {
            if let Some(saved) = saved {
                copy_entry(&latest.join(&path), &destination.join(saved))?;
            }
            let path = destination.join(path);
            if path.exists() {
                fs::remove_file(path)?;
                stats.repaired_files += 1;
            }
        }
        filesystem::ancestors(destination, log)?;
        copy_entry(&latest.join(log), &destination.join(log))?;
        if !self.log_append.is_empty() {
            fs::create_dir_all(destination.join("var/log"))?;
            use std::io::Write;
            let mut logfile = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(destination.join(log))?;
            logfile.write_all(&self.log_append)?;
            logfile.sync_all()?;
        }
        if identity(&self.base)? != self.base_identity
            || identity(&self.staged)? != self.staged_identity
            || identity(&latest)? != latest_identity
        {
            return Ok(RebaseOutcome::NotReady(NotReady {
                reason: "snapshot changed during rebase; re-stage".into(),
                path: None,
            }));
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Ok(not_ready("final reconciliation budget expired", None));
        }
        filesystem::publish(destination, final_destination)?;
        // Remove only our private working snapshot, after its sealed replacement
        // exists. Failed GC is harmless and must not invalidate the candidate.
        drop(work);
        stats.total_ms = start.elapsed().as_secs_f64() * 1000.;
        let mut candidate = Candidate::new(final_destination, &latest, latest_identity, stats)?;
        candidate.preparation = Some((self.store.clone(), self.staged_identity.clone()));
        Ok(RebaseOutcome::Candidate(Box::new(candidate)))
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

fn read_log(root: &Path) -> Result<Vec<u8>> {
    let path = Path::new("var/log/pacman.log");
    filesystem::ancestors(root, path)?;
    match fs::symlink_metadata(root.join(path)) {
        Ok(m) => {
            ensure!(m.is_file(), "pacman log must be a regular file");
            Ok(fs::read(root.join(path))?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn not_ready(reason: &str, path: Option<PathBuf>) -> RebaseOutcome {
    RebaseOutcome::NotReady(NotReady {
        reason: reason.into(),
        path,
    })
}

/// Background fallback for ANY completed transaction shape. The callback has
/// the same executor requirements as `PreparedUpdate::refresh`. This also works
/// when a transaction cannot be represented by the regular-file optimization.
/// Pacman's database stays inside the root at its usual location.
pub fn restage(
    latest: &Path,
    destination: &Path,
    apply: impl FnOnce(&Path) -> Result<()>,
) -> Result<Candidate> {
    let start = Instant::now();
    let latest = frozen(latest)?;
    let before = identity(&latest)?;
    ensure!(
        fs::symlink_metadata(destination).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "candidate destination must not exist"
    );
    let work = filesystem::WorkingRoot::new(&latest, destination)?;
    apply(work.path()).context("native background update failed")?;
    // Reject executors which left nested mounts/subvolumes in their output.
    // This is background validation, not an activation-time scan.
    filesystem::check_tree(work.path())?;
    ensure!(
        identity(&latest)? == before,
        "latest snapshot changed during native update"
    );
    filesystem::publish(work.path(), destination)?;
    drop(work);
    Candidate::new(
        destination,
        &latest,
        before,
        RebaseStats {
            native_restage: true,
            total_ms: start.elapsed().as_secs_f64() * 1000.,
            ..Default::default()
        },
    )
}

fn self_and_parents(paths: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .flat_map(|p| {
            p.ancestors()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
        })
        .collect()
}

/// Pacman's default old/current/incoming MD5 backup decision. None means keep
/// the user's file because the package's default did not change.
fn backup_target(
    path: &Path,
    old: Option<&String>,
    current: Option<&String>,
    incoming: &String,
) -> Result<Option<PathBuf>> {
    if current.is_some() && current != Some(incoming) && current != old {
        if old == Some(incoming) {
            return Ok(None);
        }
        return Ok(Some(PathBuf::from(format!(
            "{}.pacnew",
            path.to_str().context("non-UTF8 backup path")?
        ))));
    }
    Ok(Some(path.into()))
}
