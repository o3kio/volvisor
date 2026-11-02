//! Durable-state schema version vocabulary and helpers.
//!
//! Every durable store covered by `docs/schema-compatibility.md` carries an
//! explicit schema version. Files written before that policy existed have no
//! version field; they decode as [`BASELINE_SCHEMA_VERSION`] and are otherwise
//! interpreted exactly as the current shape (the historical additions are
//! serde-defaulted optional fields).
//!
//! The rules are deliberately fail-closed: a store file declaring a newer
//! version than this build supports is refused with a typed error before any
//! state is mutated, never partially decoded. Unknown fields at a supported
//! version are likewise rejected by the stores' `deny_unknown_fields` serde
//! shape, so a decode can never silently drop durable state.

use std::path::Path;

use serde::Deserialize;

use crate::error::{ApiError, ApiErrorCode};

/// Baseline schema version: the shape written before explicit versioning was
/// added. An absent `schema_version`/`record_version` decodes as this value.
pub const BASELINE_SCHEMA_VERSION: u32 = 1;

/// Detail prefix of [`ApiError::unsupported_schema_version`].
///
/// Startup surfaces wrap the typed error, but the prefix is the stable
/// discriminator operators and tests can match without parsing prose.
pub const UNSUPPORTED_SCHEMA_VERSION_PREFIX: &str = "unsupported durable-state schema version";

/// Minimal header probe for a versioned durable store.
///
/// Deliberately does **not** deny unknown fields: the probe must be able to
/// read the version of a file written by a newer build *before* the full
/// strict decode rejects its unknown fields. That ordering is what turns a
/// future-version file into the precise "unsupported schema version" error
/// rather than a generic parse failure.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SchemaVersionProbe {
    /// Schema version declared by the file; absent means
    /// [`BASELINE_SCHEMA_VERSION`].
    #[serde(default = "baseline_schema_version")]
    pub schema_version: u32,
}

/// serde default for an absent schema version (legacy files).
#[must_use]
pub const fn baseline_schema_version() -> u32 {
    BASELINE_SCHEMA_VERSION
}

/// Refuse a store file whose declared version this build cannot serve.
///
/// `found == 0` is invalid (version numbers start at the baseline) and
/// `found > supported` is a file from a newer build. Both are typed
/// `INTERNAL` startup refusals; the caller must not truncate, rewrite or
/// reset the file.
///
/// # Errors
/// Returns [`ApiErrorCode::Internal`] for a zero or newer-than-supported
/// version.
pub fn ensure_supported_schema_version(
    store: &str,
    path: &Path,
    found: u32,
    supported: u32,
) -> Result<(), ApiError> {
    if found == 0 {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "{store} state {} declares invalid schema_version 0 (the baseline is \
                 {BASELINE_SCHEMA_VERSION}); refusing to open it",
                path.display()
            ),
        ));
    }
    if found > supported {
        return Err(ApiError::unsupported_schema_version(
            store,
            path.display(),
            found,
            supported,
        ));
    }
    Ok(())
}

impl ApiError {
    /// Typed refusal for a durable store written by a newer schema.
    ///
    /// The code is `INTERNAL` (a startup/configuration condition, not a
    /// client request error) and the detail starts with
    /// [`UNSUPPORTED_SCHEMA_VERSION_PREFIX`] so it is distinguishable from
    /// generic corruption without making the wire taxonomy claim a new
    /// public error code.
    #[must_use]
    pub fn unsupported_schema_version(
        store: &str,
        path: impl std::fmt::Display,
        found: u32,
        supported: u32,
    ) -> Self {
        Self::new(
            ApiErrorCode::Internal,
            format!(
                "{UNSUPPORTED_SCHEMA_VERSION_PREFIX}: {store} state {path} declares \
                 schema_version {found}, but this build supports at most {supported}; \
                 refusing to open it partially (install a matching or newer build, or \
                 restore a pre-upgrade backup)"
            ),
        )
    }

    /// Whether this error is the unsupported-schema-version class.
    #[must_use]
    pub fn is_unsupported_schema_version(&self) -> bool {
        self.code == ApiErrorCode::Internal
            && self.detail.starts_with(UNSUPPORTED_SCHEMA_VERSION_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_defaults_legacy_files_to_the_baseline() {
        let probe: SchemaVersionProbe = serde_json::from_str("{}").expect("legacy header parses");
        assert_eq!(probe.schema_version, BASELINE_SCHEMA_VERSION);
        let probe: SchemaVersionProbe =
            serde_json::from_str(r#"{"schema_version":1,"future_field":true}"#)
                .expect("probe tolerates newer fields");
        assert_eq!(probe.schema_version, 1);
    }

    #[test]
    fn future_and_zero_versions_are_refused_typed() {
        let path = Path::new("/var/lib/volvisor/journal/lvm-state.json");
        let future =
            ensure_supported_schema_version("lvm", path, 2, 1).expect_err("future refused");
        assert_eq!(future.code, ApiErrorCode::Internal);
        assert!(future.is_unsupported_schema_version(), "{future}");
        assert!(future.detail.contains("schema_version 2"));
        assert!(future.detail.contains("supports at most 1"));

        let zero =
            ensure_supported_schema_version("lvm", path, 0, 1).expect_err("zero is not a version");
        assert_eq!(zero.code, ApiErrorCode::Internal);
        assert!(!zero.is_unsupported_schema_version());
        assert!(zero.detail.contains("invalid schema_version 0"));
    }

    #[test]
    fn current_and_older_supported_versions_pass() {
        let path = Path::new("state.json");
        ensure_supported_schema_version("store", path, 1, 1).expect("current");
        ensure_supported_schema_version("store", path, 1, 2).expect("older is readable");
    }
}
