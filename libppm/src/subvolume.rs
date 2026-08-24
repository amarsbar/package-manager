use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Result};

const TOP: &str = "/run/pond-update/top";
const ROOT: &str = "/run/pond-update/root";
const UPDATE_SUBVOLUME: &str = "@pond-update";

pub(crate) struct UpdateRoot {
    mounts: Vec<PathBuf>,
}

impl UpdateRoot {
    pub(crate) fn prepare() -> Result<Self> {
        fs::create_dir_all(TOP)?;
        fs::create_dir_all(ROOT)?;

        let device = output("findmnt", &["-n", "--nofsroot", "-o", "SOURCE", "/"])?;
        let subvolume_path = format!("{TOP}/{UPDATE_SUBVOLUME}");
        let mut update_root = Self { mounts: Vec::new() };

        update_root.mount(&["-o", "subvolid=5", &device, TOP], TOP)?;
        if !Path::new(&subvolume_path).exists() {
            run("btrfs", &["subvolume", "snapshot", "/", &subvolume_path])?;
        }
        update_root.mount(&["--bind", &subvolume_path, ROOT], ROOT)?;

        Ok(update_root)
    }

    pub(crate) fn path(&self) -> &Path {
        Path::new(ROOT)
    }

    fn mount(&mut self, args: &[&str], target: &str) -> Result<()> {
        run("mount", args)?;
        self.mounts.push(PathBuf::from(target));
        Ok(())
    }
}

impl Drop for UpdateRoot {
    fn drop(&mut self) {
        for target in self.mounts.iter().rev() {
            let _ = Command::new("umount").arg(target).status();
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("{program} failed with {status}");
    }

    Ok(())
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let result = Command::new(program).args(args).output()?;
    if !result.status.success() {
        bail!("{program} failed with {}", result.status);
    }

    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}
