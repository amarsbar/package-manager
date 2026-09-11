//! Conservative equality proof, NOT a send-stream replayer. The kernel's
//! incremental send compares Btrfs trees without reading file payloads.
//! https://btrfs.readthedocs.io/en/latest/dev/dev-send-stream.html
use super::Validation;
use anyhow::{ensure, Context, Result};
use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const MAX_STREAM: u64 = 4 * 1024 * 1024;

struct Sender(Option<Child>);
impl Drop for Sender {
    fn drop(&mut self) {
        // Do not wait synchronously for a killed process stuck in kernel I/O.
        // try_wait() caches/reaps an exited child, so kill cannot target a reused PID.
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub(super) fn compare(before: &Path, after: &Path, budget: Duration) -> Result<Validation> {
    let Some(bytes) = stream(before, after, budget)? else {
        return Ok(Validation::TimedOut);
    };
    Ok(if unchanged(&bytes)? {
        Validation::Ready
    } else {
        Validation::Changed
    })
}

pub(super) fn stream(before: &Path, after: &Path, budget: Duration) -> Result<Option<Vec<u8>>> {
    stream_limited(before, after, budget, MAX_STREAM)
}

pub(super) fn stream_limited(
    before: &Path,
    after: &Path,
    budget: Duration,
    limit: u64,
) -> Result<Option<Vec<u8>>> {
    let start = Instant::now();
    if budget.is_zero() {
        return Ok(None);
    }
    let mut child = Command::new("/usr/bin/btrfs")
        // Protocol 1 omits inode flags (e.g. immutable), so it cannot prove that
        // an update would behave the same. Require protocol 2; never downgrade.
        .args(["send", "--proto", "2", "--no-data", "-p"])
        .arg(before)
        .arg(after)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting Btrfs candidate validation")?;
    let output = child.stdout.take().context("missing Btrfs output")?;
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(result);
    });
    // Keep the child unreaped until we either observe completion or kill it.
    let mut child = Sender(Some(child));
    let Some(remaining) = budget.checked_sub(start.elapsed()) else {
        return Ok(None);
    };
    let bytes = match receive.recv_timeout(remaining) {
        Ok(result) => result.context("reading Btrfs validation stream")?,
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("Btrfs reader stopped"),
    };
    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    // stdout is closed, so the send is done; only exit bookkeeping remains.
    let status = loop {
        if let Some(status) = child.0.as_mut().unwrap().try_wait()? {
            break status;
        }
        if start.elapsed() >= budget {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(1));
    };
    ensure!(
        status.success(),
        "Btrfs candidate validation failed ({status}); leave update pending"
    );
    Ok((start.elapsed() < budget).then_some(bytes))
}

fn unchanged(bytes: &[u8]) -> Result<bool> {
    ensure!(
        bytes.len() >= 17 && &bytes[..13] == b"btrfs-stream\0",
        "invalid Btrfs stream header"
    );
    ensure!(
        u32::from_le_bytes(bytes[13..17].try_into()?) == 2,
        "unexpected Btrfs stream protocol"
    );
    let mut remaining = &bytes[17..];
    let mut started = false;
    while !remaining.is_empty() {
        ensure!(remaining.len() >= 10, "truncated Btrfs command");
        let length = u32::from_le_bytes(remaining[..4].try_into()?) as usize;
        let command = u16::from_le_bytes(remaining[4..6].try_into()?);
        ensure!(
            length <= 65536 && remaining.len() >= 10 + length,
            "invalid Btrfs command length"
        );
        // No stream checksum: this is a pipe from the local kernel, not a
        // transported stream. (A corrupt command could in principle parse as an
        // allowed root-timestamp command; that needs memory corruption first.)
        let payload = &remaining[10..10 + length];
        remaining = &remaining[10 + length..];
        if !started {
            ensure!(command == 2, "expected an incremental Btrfs snapshot");
            started = true;
            continue;
        }
        if command == 21 {
            ensure!(
                payload.is_empty() && remaining.is_empty(),
                "invalid Btrfs stream end"
            );
            return Ok(true);
        }
        // Taking a snapshot may emit times for the root itself. Root directory
        // timestamps are not part of our filesystem equivalence. Every other
        // operation, including file timestamps, forces background reconciliation.
        if command != 20 {
            return Ok(false);
        }
        let mut attrs = payload;
        let mut root_path = false;
        while !attrs.is_empty() {
            ensure!(attrs.len() >= 4, "truncated Btrfs attribute");
            let kind = u16::from_le_bytes(attrs[..2].try_into()?);
            let length = u16::from_le_bytes(attrs[2..4].try_into()?) as usize;
            ensure!(attrs.len() >= 4 + length, "invalid Btrfs attribute length");
            if kind == 15 {
                let path = &attrs[4..4 + length];
                root_path = path.is_empty() || path == b".";
            }
            attrs = &attrs[4 + length..];
        }
        if !root_path {
            return Ok(false);
        }
    }
    anyhow::bail!("Btrfs stream ended without a completion marker")
}
