use anyhow::{Context, Result};
use libflatpak::prelude::*;

use crate::InstallOutcome;

const REMOTE: &str = "flathub";
const BRANCH: &str = "stable";

pub(crate) fn install(package: &str) -> Result<InstallOutcome> {
    let installation = libflatpak::Installation::new_system(None::<&libflatpak::gio::Cancellable>)
        .context("opening the system Flatpak installation")?;
    let architecture = libflatpak::default_arch().context("determining Flatpak architecture")?;
    let reference = format!("app/{package}/{architecture}/{BRANCH}");
    let transaction = libflatpak::Transaction::for_installation(
        &installation,
        None::<&libflatpak::gio::Cancellable>,
    )
    .context("creating a Flatpak transaction")?;

    match transaction.add_install(REMOTE, &reference, &[]) {
        Ok(()) => {}
        Err(error) if error.matches(libflatpak::Error::AlreadyInstalled) => {
            return Ok(InstallOutcome::AlreadyInstalled);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("adding {reference} from {REMOTE} to the transaction"));
        }
    }

    match transaction.run(None::<&libflatpak::gio::Cancellable>) {
        Ok(()) => Ok(InstallOutcome::Installed),
        Err(error) if error.matches(libflatpak::Error::AlreadyInstalled) => {
            Ok(InstallOutcome::AlreadyInstalled)
        }
        Err(error) => Err(error).with_context(|| format!("installing {reference} from {REMOTE}")),
    }
}
