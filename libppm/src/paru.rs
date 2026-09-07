use std::{
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};

use crate::InstallOutcome;

pub(crate) fn install(package: &str) -> Result<InstallOutcome> {
    let status = Command::new("paru")
        .args([
            "--sudo",
            "pkexec",
            "--skipreview",
            "--pgpfetch",
            "--useask",
            "-S",
            "--needed",
            "--noconfirm",
            "--ask",
            "4",
            "--",
        ])
        .arg(package)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("starting Paru to install {package}"))?;

    if status.success() {
        return Ok(InstallOutcome::Installed);
    }

    bail!("installing {package} with Paru failed ({status})");
}

pub(crate) fn update(root: &Path, user: &str) -> Result<()> {
    let status = Command::new("arch-chroot")
        .args(["-S", "-u", user])
        .arg(root)
        .args([
            "/usr/bin/env",
            "HOME=/tmp",
            "/usr/bin/paru",
            "--sudo",
            "/usr/bin/pkexec",
            "--sudoflags",
            "/usr/bin/env SNAP_PAC_SKIP=y",
            "--nosudoloop",
            "-Syu",
            "--skipreview",
            "--noupgrademenu",
            "--nonewsonupgrade",
            "--noconfirm",
            "--useask",
            "--ask",
            "4",
            "--pgpfetch",
            "--failfast",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("starting Paru in {}", root.display()))?;

    if status.success() {
        return Ok(());
    }

    bail!("updating {} with Paru failed ({status})", root.display());
}
