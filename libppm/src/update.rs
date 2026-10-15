use std::thread;

use anyhow::{anyhow, bail, Result};

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
