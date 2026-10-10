//! `campaign-summary` — the P5 campaign's standalone renderer
//! (plan §6, stage D): renders the final `REPORT.md` from run
//! directories' records ALONE — the report is reproducible from the
//! records (§6), with no live rig and no test state.
//!
//! ```text
//! campaign-summary [OPTIONS] [RUN_DIR]...
//!
//!   RUN_DIR...        the run directories to merge (records dedupe
//!                     by scenario; the latest run wins; mixed
//!                     commits are flagged in the provenance)
//!   --root <DIR>      where to find run directories when none are
//!                     given (default: <workspace>/target/
//!                     campaign-evidence — the newest run directory)
//!   --all             merge EVERY run directory under the root
//!                     (the whole evidence tree; mixed commits are
//!                     flagged, never averaged)
//!   --out <FILE>      write the report to FILE (default: stdout)
//!   --check           exit with a failure if any §11 completion
//!                     gate (CG1-CG5) is incomplete — the gates as a
//!                     process exit code
//! ```
//!
//! WHAT A FULL CERTIFICATION IS (the comprehensive review's NOTE-4,
//! on the same root as the mixed-commit gate): each `cargo test`
//! binary process writes its OWN run directory, so the DEFAULT
//! (newest-run) mode renders ONE test-binary's partial run — useful
//! while iterating, never a certification. A full-suite
//! certification is `--all --check` over the whole evidence tree
//! after a FRESH full-suite run at ONE commit (a clean tree, or one
//! whose older runs the newest sweep fully re-emits): the gates
//! require every winning record to carry the same commit, and a
//! tree that mixes revisions reads INCOMPLETE with the commits
//! named — re-run the suite and certify again.
//!
//! The claim discipline (§0/§6) is the report's header, verbatim:
//! Tier S proves the implemented logic's behavior under the bounded
//! injected fault space; it proves nothing about real media, real
//! DRBD, or a real VMM; production support is not claimed.

// Test-support crate (see the lib's header for the convention):
// this tool stops on a broken environment with its context, never
// a half-rendered report — the panics ARE the diagnostics.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use volvisor_campaign::summary;

/// The usage line (stderr).
const USAGE: &str =
    "usage: campaign-summary [--root <DIR>] [--all] [--out <FILE>] [--check] [RUN_DIR]...";

/// The entry point: parse, discover, render, (optionally) gate.
fn main() -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut root: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut all = false;
    let mut check = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                let Some(value) = arguments.next() else {
                    eprintln!("campaign-summary: --root needs a directory\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                root = Some(PathBuf::from(value));
            }
            "--out" => {
                let Some(value) = arguments.next() else {
                    eprintln!("campaign-summary: --out needs a file\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                out = Some(PathBuf::from(value));
            }
            "--all" => all = true,
            "--check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("campaign-summary: unknown option {other}\n{USAGE}");
                return ExitCode::FAILURE;
            }
            other => dirs.push(PathBuf::from(other)),
        }
    }

    // Discover run directories when none were given: every run
    // directory under the root (`--all`), else the newest.
    if dirs.is_empty() {
        let root = root.unwrap_or_else(default_root);
        let discovered = discover_run_dirs(&root);
        if discovered.is_empty() {
            eprintln!(
                "campaign-summary: no run directories under {} (run the campaign first)",
                root.display()
            );
            return ExitCode::FAILURE;
        }
        if all {
            dirs = discovered;
        } else {
            dirs = vec![discovered[discovered.len() - 1].clone()];
        }
    }
    for dir in &dirs {
        if !dir.is_dir() {
            eprintln!("campaign-summary: {} is not a directory", dir.display());
            return ExitCode::FAILURE;
        }
    }

    let report = summary::build_campaign_report(&dirs);
    if check {
        // The gates must be computed over exactly what was rendered;
        // the cheapest honest route is to re-collect through the
        // same builder's inputs.
        let incomplete = summary::completion_gates_over(&dirs)
            .iter()
            .filter(|gate| !gate.complete)
            .count();
        if incomplete > 0 {
            eprintln!(
                "campaign-summary: {incomplete} completion gate(s) incomplete over the \
                 given run directories (see the report's §11 section)"
            );
            write_report(&report, out);
            return ExitCode::FAILURE;
        }
    }
    write_report(&report, out);
    ExitCode::SUCCESS
}

/// Write the report to `out` or stdout.
fn write_report(report: &str, out: Option<PathBuf>) {
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("output dir");
            }
            std::fs::write(&path, report).expect("write report");
        }
        None => println!("{report}"),
    }
}

/// The default evidence root: `<workspace>/target/campaign-
/// evidence` (the crate sits one level under `crates/`).
fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("campaign-evidence")
}

/// The run directories under `root`, in ascending modified-time
/// order (`run-*` directories only; the merge's latest-wins rule
/// depends on this order).
fn discover_run_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(u128, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        })
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis());
            (modified, path)
        })
        .collect();
    dirs.sort_by_key(|(modified, _)| *modified);
    dirs.into_iter().map(|(_, path)| path).collect()
}
