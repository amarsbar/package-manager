use std::{process::Command, thread};

use anyhow::{anyhow, bail, Context, Result};

pub fn updates_available() -> Result<bool> {
    let output = Command::new("checkupdates")
        .output()
        .context("running checkupdates")?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).lines().count() > 3),
        Some(2) => Ok(false),
        _ => bail!(
            "checkupdates failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub fn update() -> Result<()> {
    let (paru, flatpak) = thread::scope(|scope| {
        let paru = scope.spawn(crate::paru::update);
        let flatpak = crate::flatpak::update();
        let paru = paru
            .join()
            .unwrap_or_else(|_| Err(anyhow!("Paru update worker panicked")));
        (paru, flatpak)
    });

    match (paru, flatpak) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error.context("Paru update failed")),
        (Ok(()), Err(error)) => Err(error.context("Flatpak update failed")),
        (Err(paru), Err(flatpak)) => {
            bail!("Paru update failed: {paru:#}\nFlatpak update failed: {flatpak:#}")
        }
    }
}
