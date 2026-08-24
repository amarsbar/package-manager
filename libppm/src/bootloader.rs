use std::{
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, ensure, Context, Result};

const UPDATE_ENTRY: &str = "pond-update";

pub(crate) fn set_next_boot(root: &Path) -> Result<()> {
    for artifact in ["boot/vmlinuz-linux-pond", "boot/initramfs-linux-pond.img"] {
        let artifact = root.join(artifact);
        ensure!(artifact.is_file(), "{} is missing", artifact.display());
    }

    let status = Command::new("grub-reboot")
        .arg(UPDATE_ENTRY)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("selecting GRUB entry to updated root")?;

    if !status.success() {
        bail!("selecting GRUB entry {UPDATE_ENTRY} failed ({status})");
    }

    Ok(())
}
