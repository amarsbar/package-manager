use std::{
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, ensure, Context, Result};

use crate::Slot;

pub(crate) fn set_next_boot(root: &Path, slot: Slot) -> Result<()> {
    for artifact in ["boot/vmlinuz-linux-pond", "boot/initramfs-linux-pond.img"] {
        let artifact = root.join(artifact);
        ensure!(artifact.is_file(), "{} is missing", artifact.display());
    }

    let entry = match slot {
        Slot::A => "pond-slot-a",
        Slot::B => "pond-slot-b",
    };

    let status = Command::new("grub-reboot")
        .arg(entry)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("selecting GRUB entry to updated root")?;

    if !status.success() {
        bail!("selecting GRUB entry {entry} failed ({status})");
    }

    Ok(())
}
