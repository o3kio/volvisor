//! Compile the version stamp (P7-A, ADR-0009): the output of
//! `git describe --tags --always --dirty` over the enclosing checkout,
//! exported to the crate as `VOLVISORD_BUILD_DESCRIBE` (the lib's
//! [`volvisord::version`] resolves the final stamp; the crate version
//! is the fallback). Per ADR-0009's rule, version stamping never
//! fails the build: every failure path here degrades to the fallback.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let git_dir = enclosing_git_dir(&manifest_dir);
    // Rerun policy: the stamp must not go stale, in both of its
    // components. The commit/tag side is repinned by the git-path
    // watches — HEAD (branch switches), refs/heads (new commits),
    // refs/tags and packed-refs (tag changes), each best-effort: a
    // path that does not exist simply never triggers. The -dirty side
    // reflects the working tree, so the package root must be watched
    // as well: once any rerun-if-changed is emitted, cargo watches
    // ONLY the listed paths, and without the package root a source
    // edit would never re-run this script and the compiled -dirty
    // suffix would go stale. Watching the package root cannot loop:
    // the script writes nothing into the package directory, and the
    // workspace target/ lives outside it (the package holds only
    // src/, tests/, Cargo.toml and this file).
    watch(Path::new("."));
    if let Some(git_dir) = &git_dir {
        watch(&git_dir.join("HEAD"));
        watch(&git_dir.join("packed-refs"));
        watch(&git_dir.join("refs/heads"));
        watch(&git_dir.join("refs/tags"));
    }
    if let Some(stamp) = describe(&manifest_dir) {
        println!("cargo:rustc-env=VOLVISORD_BUILD_DESCRIBE={stamp}");
    }
}

/// Emit `cargo:rerun-if-changed` for `path`.
fn watch(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

/// The `.git` directory of the checkout enclosing this crate: the
/// plain directory in a normal checkout, or the `gitdir:` target when
/// `.git` is a linked-worktree pointer file. `None` when there is no
/// `.git` at all (a tarball build — the stamp falls back to the crate
/// version).
fn enclosing_git_dir(manifest_dir: &Path) -> Option<PathBuf> {
    let dot_git = manifest_dir.join("../..").join(".git");
    match std::fs::read_to_string(&dot_git) {
        // A linked worktree's `.git` is a file: `gitdir: <path>`.
        Ok(pointer) => pointer
            .strip_prefix("gitdir:")
            .map(|target| PathBuf::from(target.trim())),
        Err(_) if dot_git.is_dir() => Some(dot_git),
        Err(_) => None,
    }
}

/// Run `git describe --tags --always --dirty` inside `manifest_dir`,
/// accepting the output only when it provably ran in THIS repository:
/// `rev-parse --show-toplevel` must name the workspace root, so a
/// vendored copy inside another repository falls back rather than
/// stamping that repository's describe. `None` on any failure — git
/// missing, no checkout, a non-UTF8 stamp — is the fallback signal.
fn describe(manifest_dir: &Path) -> Option<String> {
    if !toplevel_is_workspace(manifest_dir) {
        return None;
    }
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stamp = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if stamp.is_empty() { None } else { Some(stamp) }
}

/// Whether git's idea of the repository root is this workspace.
fn toplevel_is_workspace(manifest_dir: &Path) -> bool {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(manifest_dir)
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(toplevel) = String::from_utf8(output.stdout) else {
        return false;
    };
    let toplevel = PathBuf::from(toplevel.trim());
    let workspace = manifest_dir.join("../..");
    match (
        std::fs::canonicalize(&toplevel),
        std::fs::canonicalize(&workspace),
    ) {
        (Ok(a), Ok(b)) => a == b,
        // Canonicalization fails only for exotic layouts; the raw
        // comparison is the best remaining check and errs toward the
        // fallback.
        _ => toplevel == workspace,
    }
}
