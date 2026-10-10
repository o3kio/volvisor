//! The version stamp (P7-A, ADR-0009): what `--version` prints on
//! both binaries.
//!
//! The stamp is compiled in by the crate's `build.rs` from
//! `git describe --tags --always --dirty` run in the repository
//! checkout at build time; a build outside a checkout (a source
//! tarball, a vendored copy inside another repository) falls back to
//! the crate version rather than stamping a foreign or missing
//! describe. Both paths are honest about what they name: a describe
//! names the exact source state (the abbreviated commit hash while
//! the repository carries no tags, per `--always`); the fallback names
//! only the crate's version.

/// Resolve the version stamp from the build context: the build-time
/// `git describe --tags --always --dirty` output when one was
/// recorded, else the crate version. A `const fn` so the compiled-in
/// stamp and its tests share one source of truth. An empty describe
/// (never emitted by `build.rs`; defended against here) counts as
/// absent.
#[must_use]
pub const fn stamp<'a>(describe: Option<&'a str>, pkg_version: &'a str) -> &'a str {
    match describe {
        Some(describe) if !describe.is_empty() => describe,
        _ => pkg_version,
    }
}

/// The compiled-in version stamp (what `--version` prints): the
/// repository's `git describe --tags --always --dirty` at build time,
/// or the crate version when the build ran outside a checkout.
pub const VERSION: &str = stamp(
    option_env!("VOLVISORD_BUILD_DESCRIBE"),
    env!("CARGO_PKG_VERSION"),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_stamp_is_non_empty() {
        assert_ne!(VERSION, "");
    }

    #[test]
    fn stamp_prefers_the_describe_output() {
        // The tagged shape (`<tag>-<commits>-g<hash>[-dirty]`).
        assert_eq!(
            stamp(Some("v0.1.0-12-g3b3934b-dirty"), "0.1.0"),
            "v0.1.0-12-g3b3934b-dirty"
        );
        // The --always shape (no reachable tags): the abbreviated hash.
        assert_eq!(stamp(Some("3b3934b"), "0.1.0"), "3b3934b");
    }

    #[test]
    fn stamp_falls_back_to_the_crate_version() {
        // No git, no checkout, or git failed: the crate version.
        assert_eq!(stamp(None, "0.1.0"), "0.1.0");
        // An empty describe is no describe (defensive; build.rs trims
        // and never emits one).
        assert_eq!(stamp(Some(""), "0.1.0"), "0.1.0");
    }

    /// The compiled-in stamp must match whichever source the build
    /// provided: inside the repository (the normal case, including
    /// CI) the describe output is present and is the single token
    /// `git describe --tags --always --dirty` produces; a tarball
    /// build exercises the fallback instead. Both are honest.
    #[test]
    fn the_compiled_in_stamp_matches_its_build_source() {
        match option_env!("VOLVISORD_BUILD_DESCRIBE") {
            Some(describe) if !describe.is_empty() => {
                assert_eq!(VERSION, describe);
                assert!(
                    !describe.chars().any(char::is_whitespace),
                    "the describe output must be a single token: {describe:?}"
                );
            }
            _ => assert_eq!(VERSION, env!("CARGO_PKG_VERSION")),
        }
    }
}
