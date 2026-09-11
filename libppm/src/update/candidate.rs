use super::{
    filesystem::{frozen, identity, SnapshotIdentity},
    RebaseStats,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// A sealed private root together with the exact state it was reconciled against.
/// This proves neither bootability nor that a running system has stopped writing.
#[derive(Debug, Serialize, Deserialize)]
pub struct Candidate {
    path: PathBuf,
    identity: SnapshotIdentity,
    latest: PathBuf,
    latest_identity: SnapshotIdentity,
    #[serde(default)]
    pub(super) preparation: Option<(PathBuf, SnapshotIdentity)>,
    pub stats: RebaseStats,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Validation {
    Ready,
    Changed,
    TimedOut,
}

impl Candidate {
    /// Capture an already completed native transaction of ANY shape, including
    /// hooks, scriptlets, replacements and removals. The caller attests that
    /// `completed` is the successful output on exactly `base`, with no concurrent
    /// writers or writable live shared mounts. Both inputs must be frozen.
    /// This does not authorize activation: validate against the final root first.
    pub fn from_completed(base: &Path, completed: &Path) -> Result<Self> {
        let base = frozen(base)?;
        let completed = frozen(completed)?;
        let before = identity(&base)?;
        let after = identity(&completed)?;
        super::filesystem::check_tree(&base)?;
        super::filesystem::check_tree(&completed)?;
        ensure!(
            identity(&base)? == before && identity(&completed)? == after,
            "completed transaction input changed during capture"
        );
        let candidate = Self::new(&completed, &base, before, RebaseStats::default())?;
        ensure!(
            candidate.identity == after,
            "completed root changed during capture"
        );
        Ok(candidate)
    }

    pub(super) fn new(
        path: &Path,
        latest: &Path,
        expected: SnapshotIdentity,
        stats: RebaseStats,
    ) -> Result<Self> {
        let path = frozen(path)?;
        let latest = frozen(latest)?;
        ensure!(
            identity(&latest)? == expected,
            "reference changed during candidate publication"
        );
        Ok(Self {
            identity: identity(&path)?,
            latest_identity: expected,
            preparation: None,
            path,
            latest,
            stats,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Save a receipt outside the candidate, without replacing another receipt.
    /// Retain both the candidate and its latest-state snapshot until activation.
    pub fn save(&self, receipt: &Path) -> Result<()> {
        self.check_identities()?;
        let parent = receipt
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(receipt)
            .context("publishing candidate receipt")?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn open(receipt: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(receipt)?;
        ensure!(metadata.is_file() && metadata.mode() & 0o077 == 0
                && metadata.uid() == unsafe { libc::geteuid() },
            "candidate receipt must be a private regular file owned by the updater's effective user");
        ensure!(
            metadata.len() <= 64 * 1024,
            "candidate receipt is too large"
        );
        let candidate: Self = serde_json::from_slice(&fs::read(receipt)?)?;
        candidate.check_identities()?;
        Ok(candidate)
    }

    pub(super) fn check_identities(&self) -> Result<()> {
        ensure!(
            identity(&frozen(&self.path)?)? == self.identity,
            "candidate changed after publication"
        );
        ensure!(
            identity(&frozen(&self.latest)?)? == self.latest_identity,
            "candidate's reference snapshot changed"
        );
        Ok(())
    }

    pub(super) fn reference(&self) -> &Path {
        &self.latest
    }

    /// Validate against a FRESH read-only snapshot taken after root writers have
    /// stopped. They must remain stopped until boot selection. Never pass an old
    /// snapshot as evidence of the live root's current state.
    ///
    /// Does not hash the root, copy files, execute pacman, or select a boot entry.
    /// A changed tree or expired budget leaves the update pending. I/O errors
    /// likewise do not authorize activation. The OS is not a real-time system:
    /// the budget limits waiting for the diff worker, not kernel I/O stalls.
    pub fn validate(&self, final_root: &Path, budget: Duration) -> Result<Validation> {
        let start = std::time::Instant::now();
        if budget.is_zero() {
            return Ok(Validation::TimedOut);
        }
        self.check_identities()?;
        let final_root = frozen(final_root)?;
        let final_identity = identity(&final_root)?;
        let Some(remaining) = budget.checked_sub(start.elapsed()) else {
            return Ok(Validation::TimedOut);
        };
        let result = if final_identity == self.latest_identity {
            Validation::Ready
        } else {
            super::validation::compare(&self.latest, &final_root, remaining)?
        };
        self.check_identities()?;
        ensure!(
            identity(&final_root)? == final_identity,
            "final snapshot changed during validation"
        );
        if start.elapsed() >= budget {
            return Ok(Validation::TimedOut);
        }
        Ok(result)
    }
}
