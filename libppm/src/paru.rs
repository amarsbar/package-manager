use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};

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
            "--"
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