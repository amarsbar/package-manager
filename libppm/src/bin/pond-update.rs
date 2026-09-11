//! Explicit-path developer/operator interface. No host defaults or boot writes.
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

use anyhow::{bail, Result};
use libppm::update::{Candidate, PreparedUpdate, RebaseOutcome, Validation};
use serde_json::json;

const USAGE: &str = "pond-update prepare BASE STAGED DB_RELATIVE STORE [PACKAGE...]\n\
pond-update rebase STORE LATEST DESTINATION [RECEIPT]\n\
pond-update finish STORE RECEIPT FINAL_SNAPSHOT DESTINATION TIMEOUT_MS [NEW_RECEIPT]\n\
pond-update capture BASE COMPLETED RECEIPT\n\
pond-update validate RECEIPT FINAL_SNAPSHOT TIMEOUT_MS\n\
Inputs must be private read-only Btrfs snapshots.\n\
This builds candidates only; it does not run pacman or select a boot entry.\n\
FINAL_SNAPSHOT must be fresh, taken after root writers stop and remain stopped.\n\
Exit codes: 0 completed, 3 not ready (re-stage in background), 1 error.";

fn execute(args: Vec<OsString>) -> Result<ExitCode> {
    let command = args.first().and_then(|a| a.to_str());
    if matches!(command, Some("--help" | "-h")) && args.len() == 1 {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let paths: Vec<_> = args.iter().skip(1).map(PathBuf::from).collect();
    match command {
        Some("prepare") if paths.len() >= 4 => {
            PreparedUpdate::prepare(&paths[0], &paths[1], &paths[4..], &paths[2], &paths[3])?;
            println!("{}", json!({"status": "prepared", "store": paths[3]}));
            Ok(ExitCode::SUCCESS)
        }
        Some("rebase") if matches!(paths.len(), 3 | 4) => {
            let update = PreparedUpdate::open(&paths[0])?;
            match update.rebase(&paths[1], &paths[2])? {
                RebaseOutcome::Candidate(candidate) => {
                    if let Some(receipt) = paths.get(3) {
                        candidate.save(receipt)?;
                    }
                    println!(
                        "{}",
                        json!({"status": "candidate", "path": candidate.path(), "stats": candidate.stats})
                    );
                    Ok(ExitCode::SUCCESS)
                }
                RebaseOutcome::NotReady(reason) => {
                    println!(
                        "{}",
                        json!({"status": "not-ready", "reason": reason.reason, "path": reason.path})
                    );
                    Ok(ExitCode::from(3))
                }
            }
        }
        Some("capture") if paths.len() == 3 => {
            let candidate = Candidate::from_completed(&paths[0], &paths[1])?;
            candidate.save(&paths[2])?;
            println!(
                "{}",
                json!({"status": "candidate", "path": candidate.path(), "receipt": paths[2]})
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("finish") if matches!(paths.len(), 5 | 6) => {
            let update = PreparedUpdate::open(&paths[0])?;
            let candidate = Candidate::open(&paths[1])?;
            let budget = paths[4]
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid timeout"))?
                .parse::<u64>()?;
            match update.finish(
                &candidate,
                &paths[2],
                &paths[3],
                std::time::Duration::from_millis(budget),
            )? {
                RebaseOutcome::Candidate(candidate) => {
                    if let Some(receipt) = paths.get(5) {
                        candidate.save(receipt)?;
                    }
                    println!(
                        "{}",
                        json!({"status": "candidate", "path": candidate.path(), "stats": candidate.stats})
                    );
                    Ok(ExitCode::SUCCESS)
                }
                RebaseOutcome::NotReady(reason) => {
                    println!(
                        "{}",
                        json!({"status": "not-ready", "reason": reason.reason, "path": reason.path})
                    );
                    Ok(ExitCode::from(3))
                }
            }
        }
        Some("validate") if paths.len() == 3 => {
            let budget = paths[2]
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid timeout"))?
                .parse::<u64>()?;
            let candidate = Candidate::open(&paths[0])?;
            let result = candidate.validate(&paths[1], std::time::Duration::from_millis(budget))?;
            let status = match result {
                Validation::Ready => "ready",
                Validation::Changed => "changed",
                Validation::TimedOut => "timed-out",
            };
            println!("{}", json!({"status": status, "path": candidate.path()}));
            Ok(if result == Validation::Ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            })
        }
        _ => bail!("{USAGE}"),
    }
}

fn main() -> ExitCode {
    match execute(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!(
                "{}",
                json!({"status": "error", "error": format!("{error:#}")})
            );
            ExitCode::FAILURE
        }
    }
}
