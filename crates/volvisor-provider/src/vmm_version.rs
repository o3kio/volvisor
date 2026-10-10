//! The Cloud Hypervisor version gate (P6-B, ADR-0006 first slice
//! part 1): the pinned, startup-verified version below which the
//! grow-notification of an attached volume is refused.
//!
//! # Why a minimum exists
//!
//! Upstream cloud-hypervisor added support for resizing **externally
//! grown** host block devices (upstream PR #7948): before that
//! change, a `vm.resize-disk` call on a disk whose host backing grew
//! outside the VMM does not behave as the grow-notification contract
//! needs. Volvisor therefore refuses to notify a version it has not
//! proven meets the configured minimum — the refusal is typed, the
//! reason recorded, and the grow still reports
//! `guest_notification_status: retry_required` (never a silent
//! un-notified success). The repository deliberately pins no default
//! release: the operator configures `vmm.minimum_version` from the
//! release they have actually qualified.
//!
//! # The probe
//!
//! At daemon startup the gate runs `<cloud_hypervisor> --version`
//! through the injected [`CommandRunner`] (argv-exact testable, the
//! same shell-free path every external command takes), parses the
//! version **tolerantly** (cloud-hypervisor prints like
//! `cloud-hypervisor v37.0`; the parser scans the output's tokens
//! for the first that parses, so exact spellings and trailing
//! annotation do not matter), compares it against the configured
//! minimum, and caches the verdict for the daemon's lifetime.
//!
//! The fail-closed default is the point: no binary configured, no
//! minimum configured, an unrunnable binary, a failed probe or an
//! unparseable output all cache a **refused** verdict with the
//! recorded reason — the notification is never attempted on an
//! unproven version. A refusal is a runtime posture, not a startup
//! error: the daemon serves, and every attached grow records
//! `retry_required` with the reason until the configuration is
//! fixed and the daemon restarted.
//!
//! Semver precedence follows the specification: numeric
//! `(major, minor, patch)` comparison, then pre-release ordering (a
//! release outranks any pre-release of the same triple; pre-release
//! identifiers compare numerically when both numeric, lexically
//! otherwise, numeric below alphanumeric; fewer identifiers rank
//! below more when the shared prefix is equal). Build metadata
//! (`+...`) is parsed and ignored for precedence, as the
//! specification directs.

use std::path::Path;

use volvisor_types::{ApiError, ApiErrorCode};

use crate::runner::CommandRunner;

/// One semantic version: `major.minor.patch[-pre][+build]`, with
/// build metadata retained for display but ignored for precedence
/// (both in ordering and in equality — see the `PartialEq` impl).
#[derive(Clone, Debug, Eq)]
pub struct Semver {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
    /// Patch version.
    pub patch: u64,
    /// The pre-release identifiers (`None` for a release).
    pub pre: Option<String>,
    /// Build metadata, display-only (never part of precedence).
    pub build: Option<String>,
}

impl Semver {
    /// Parse `text` as a semantic version, tolerantly: an optional
    /// leading `v`, `MAJOR[.MINOR[.PATCH]]` with missing components
    /// defaulting to `0`, an optional `-pre` and `+build`.
    ///
    /// # Errors
    /// `Err(detail)` naming what failed to parse — never a guess.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let without_v = trimmed.strip_prefix('v').unwrap_or(trimmed);
        // Split off build metadata first (+ binds loosest in the
        // spelling), then the pre-release from the numeric core.
        let (core_and_pre, build) = split_once_char(without_v, '+');
        let (core, pre) = split_once_char(core_and_pre, '-');
        let mut numbers = core.split('.');
        // `MAJOR[.MINOR[.PATCH]]`: a missing component defaults to 0
        // (so `37` is `37.0.0`), while a *present but empty* or
        // non-numeric component (`37.` or `37.x`) is a parse error —
        // never a guess.
        let parse_number = |part: Option<&str>, what: &str| -> Result<u64, String> {
            let Some(part) = part else {
                return Ok(0);
            };
            if part.is_empty() {
                return Err(format!("the {what} component is empty"));
            }
            part.parse::<u64>()
                .map_err(|_| format!("the {what} component {part:?} is not a number"))
        };
        let major = parse_number(numbers.next(), "major")?;
        let minor = parse_number(numbers.next(), "minor")?;
        let patch = parse_number(numbers.next(), "patch")?;
        if numbers.next().is_some() {
            return Err("more than three numeric components".to_owned());
        }
        let validate_identifiers = |text: &str| -> Result<String, String> {
            if text.is_empty() || text.starts_with('.') || text.ends_with('.') {
                return Err(format!("invalid pre-release {text:?}"));
            }
            Ok(text.to_owned())
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre: pre.map(validate_identifiers).transpose()?,
            build: build.map(str::to_owned),
        })
    }
}

impl std::fmt::Display for Semver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

/// Precedence per the semantic versioning specification.
impl Ord for Semver {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        let numeric =
            (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if numeric != Ordering::Equal {
            return numeric;
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            // A release outranks any pre-release of the same triple.
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(ours), Some(theirs)) => compare_pre_release(ours, theirs),
        }
    }
}

impl PartialOrd for Semver {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Precedence equality, deliberately: build metadata never
/// participates (the specification's rule), so `37.0.0+one` equals
/// `37.0.0+two`.
impl PartialEq for Semver {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

/// Compare two pre-release strings identifier-by-identifier (the
/// specification's rule: numeric identifiers numerically, lexical
/// otherwise, numeric below alphanumeric, fewer below more on an
/// equal shared prefix).
fn compare_pre_release(ours: &str, theirs: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    for (ours, theirs) in ours.split('.').zip(theirs.split('.')) {
        let ordering = match (ours.parse::<u64>(), theirs.parse::<u64>()) {
            (Ok(ours), Ok(theirs)) => ours.cmp(&theirs),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => ours.cmp(theirs),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    ours.split('.').count().cmp(&theirs.split('.').count())
}

/// Split at the first `separator`, returning the whole string and
/// `None` when the separator is absent.
fn split_once_char(text: &str, separator: char) -> (&str, Option<&str>) {
    match text.find(separator) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    }
}

/// Scan a `--version` output for its version: the first
/// whitespace-separated token that parses as a [`Semver`] (a
/// leading `v` is tolerated, so `cloud-hypervisor v37.0` yields
/// `37.0.0`).
///
/// Tolerant by design: the gate must not depend on an exact output
/// format cloud-hypervisor never promised. `None` when no token
/// parses — the caller's unproven verdict.
#[must_use]
pub fn parse_version_output(output: &str) -> Option<Semver> {
    output.split_whitespace().find_map(Semver::parse_ok)
}

impl Semver {
    /// [`Semver::parse`] returning `None` instead of a detail (the
    /// tolerant output scan).
    fn parse_ok(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }
}

/// The gate's cached startup verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateVerdict {
    /// The observed version meets the configured minimum.
    Proven {
        /// The version the probe observed.
        observed: Semver,
    },
    /// Refused, with the recorded reason (the fail-closed default:
    /// every unproven posture — nothing configured, the probe
    /// failed, the output unparseable, the version below the
    /// minimum — lands here).
    Refused {
        /// The human-readable, recorded reason.
        reason: String,
    },
}

/// The startup-verified Cloud Hypervisor version gate (the module
/// docs): one probe at construction, one cached verdict for the
/// daemon's lifetime.
///
/// Constructed through [`VmmVersionGate::probe`] (the daemon's
/// startup path) or [`VmmVersionGate::refused`] (tests and wiring
/// that must simulate an unproven posture); every consumer goes
/// through [`VmmVersionGate::check`].
pub struct VmmVersionGate {
    verdict: GateVerdict,
}

impl VmmVersionGate {
    /// Probe and cache the verdict: run `<cloud_hypervisor>
    /// --version` through `runner` when both the binary and the
    /// minimum are configured, compare, cache. The banner is scanned
    /// from both output streams (stdout first; some builds print it
    /// to stderr).
    ///
    /// Nothing configured is a **refused** verdict with the recorded
    /// reason (the fail-closed default), never a proven-by-omission:
    /// when the binary or the minimum is absent the probe is not
    /// even attempted (the reason names what is missing).
    #[must_use]
    pub fn probe(
        cloud_hypervisor: Option<&Path>,
        minimum_version: Option<&str>,
        runner: &dyn CommandRunner,
    ) -> Self {
        let Some(binary) = cloud_hypervisor else {
            return Self::refused(
                "no vmm.cloud_hypervisor binary is configured; the grow-notification \
                 version gate cannot prove the VMM version",
            );
        };
        let Some(minimum_text) = minimum_version else {
            return Self::refused(
                "no vmm.minimum_version is configured; the grow-notification version \
                 gate has no pinned minimum to prove against",
            );
        };
        let minimum = match Semver::parse(minimum_text) {
            Ok(minimum) => minimum,
            Err(detail) => {
                // Defense in depth: configuration validation refuses
                // an unparseable minimum at load; a programmatic
                // config that skipped it lands here, refused.
                return Self::refused(format!(
                    "the configured vmm.minimum_version {minimum_text:?} does not \
                     parse as semver: {detail}"
                ));
            }
        };
        let output = match runner.run(&binary.display().to_string(), &["--version"]) {
            Ok(output) => output,
            Err(error) => {
                return Self::refused(format!(
                    "the cloud-hypervisor version probe failed to execute {}: {error}",
                    binary.display()
                ));
            }
        };
        if !output.success {
            return Self::refused(format!(
                "the cloud-hypervisor version probe exited unsuccessfully: {}",
                output.stderr_excerpt()
            ));
        }
        // The banner is scanned from both streams, stdout first: some
        // builds print it to stderr (a noisy init before the banner,
        // or a fully stderr banner) — the tolerant parser accepts a
        // version token on either.
        let Some(observed) =
            parse_version_output(&output.stdout).or_else(|| parse_version_output(&output.stderr))
        else {
            return Self::refused(format!(
                "the cloud-hypervisor version output could not be parsed (stdout and \
                 stderr both scanned): {:?} / {:?}",
                output.stdout, output.stderr
            ));
        };
        if observed >= minimum {
            Self {
                verdict: GateVerdict::Proven { observed },
            }
        } else {
            Self::refused(format!(
                "the observed cloud-hypervisor version {observed} is below the \
                 configured minimum {minimum}"
            ))
        }
    }

    /// A pre-refused gate (the fail-closed postures that do not
    /// probe: tests, and wiring variants that know the verdict).
    #[must_use]
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            verdict: GateVerdict::Refused {
                reason: reason.into(),
            },
        }
    }

    /// The cached verdict (introspection for wiring tests).
    #[must_use]
    pub fn verdict(&self) -> &GateVerdict {
        &self.verdict
    }

    /// Consult the cached verdict: `Ok(observed)` when proven, the
    /// typed refusal carrying the recorded reason otherwise.
    ///
    /// # Errors
    /// [`ApiError`] typed `INVALID_STATE` carrying the recorded
    /// reason — the caller records it as the notification's
    /// `retry_required` reason, never a silent un-notified success.
    pub fn check(&self) -> Result<Semver, ApiError> {
        match &self.verdict {
            GateVerdict::Proven { observed } => Ok(observed.clone()),
            GateVerdict::Refused { reason } => Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("grow-notification version gate refused: {reason}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn version(text: &str) -> Result<Semver, String> {
        Semver::parse(text)
    }

    #[test]
    fn semver_parses_the_core_shapes() {
        assert_eq!(
            version("37.0").expect("parses"),
            Semver {
                major: 37,
                minor: 0,
                patch: 0,
                pre: None,
                build: None
            }
        );
        assert_eq!(
            version("37").expect("parses"),
            Semver {
                major: 37,
                minor: 0,
                patch: 0,
                pre: None,
                build: None
            }
        );
        assert_eq!(
            version("v37.0.1").expect("parses"),
            Semver {
                major: 37,
                minor: 0,
                patch: 1,
                pre: None,
                build: None
            }
        );
        assert_eq!(
            version("36.2.0-rc1").expect("parses"),
            Semver {
                major: 36,
                minor: 2,
                patch: 0,
                pre: Some("rc1".to_owned()),
                build: None
            }
        );
        assert_eq!(
            version("37.0.0+build.1").expect("parses"),
            Semver {
                major: 37,
                minor: 0,
                patch: 0,
                pre: None,
                build: Some("build.1".to_owned())
            }
        );
    }

    #[test]
    fn semver_rejects_malformed_input() {
        for text in [
            "",
            "v",
            "37.",
            ".37",
            "37.x",
            "37.0.1.2",
            "37.0.0-",
            "37.0.0-.rc",
        ] {
            assert!(
                version(text).is_err(),
                "{text:?} must not parse as a version"
            );
        }
    }

    #[test]
    fn semver_precedence_follows_the_specification() {
        // Numeric ordering on the triple.
        assert!(version("37.0.0").expect("a") > version("36.9.9").expect("b"));
        assert!(version("37.1.0").expect("a") > version("37.0.99").expect("b"));
        assert_eq!(version("37.0").expect("a"), version("37.0.0").expect("b"));
        // A release outranks any pre-release of the same triple.
        assert!(version("37.0.0").expect("a") > version("37.0.0-rc1").expect("b"));
        // Pre-release identifiers: numeric below alphanumeric...
        assert!(version("37.0.0-1").expect("a") < version("37.0.0-alpha").expect("b"));
        // ...identifiers compare numerically only when both are
        // all-numeric (10 above 2)...
        assert!(version("37.0.0-10").expect("a") > version("37.0.0-2").expect("b"));
        // ...while alphanumeric identifiers compare lexically - so
        // rc10 sorts BELOW rc2 (the specification's rule, not a
        // decimal reading of the digits)...
        assert!(version("37.0.0-rc10").expect("a") < version("37.0.0-rc2").expect("b"));
        // ...lexical for two alphanumerics...
        assert!(version("37.0.0-alpha").expect("a") < version("37.0.0-beta").expect("b"));
        // ...fewer identifiers below more on an equal prefix.
        assert!(version("37.0.0-rc").expect("a") < version("37.0.0-rc.1").expect("b"));
        // Build metadata never participates.
        assert_eq!(
            version("37.0.0+one").expect("a"),
            version("37.0.0+two").expect("b")
        );
    }

    #[test]
    fn version_output_parsing_is_tolerant() {
        assert_eq!(
            parse_version_output("cloud-hypervisor v37.0\n"),
            Some(version("37.0").expect("parses"))
        );
        assert_eq!(
            parse_version_output("cloud-hypervisor 37.0.1 (commit abc)\r\n"),
            Some(version("37.0.1").expect("parses"))
        );
        assert_eq!(
            parse_version_output("cloud-hypervisor v36.2.0-rc1"),
            Some(version("36.2.0-rc1").expect("parses"))
        );
        assert_eq!(parse_version_output("no version here"), None);
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("cloud-hypervisor\n"), None);
    }

    /// The gate under a scripted runner plus the runner for the
    /// argv-exact assertions.
    fn gate(
        script: impl Fn(&str, &[&str]) -> Option<CommandOutput> + Send + Sync + 'static,
    ) -> (VmmVersionGate, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner::with_closure(script));
        (
            VmmVersionGate::probe(
                Some(PathBuf::from("/usr/bin/cloud-hypervisor").as_path()),
                Some("37.0.0"),
                runner.as_ref(),
            ),
            runner,
        )
    }

    #[test]
    fn a_meeting_version_proves_the_gate_with_exact_argv() {
        let (gate, runner) = gate(|program, args| {
            assert_eq!(program, "/usr/bin/cloud-hypervisor");
            assert_eq!(args, ["--version"]);
            Some(CommandOutput::success("cloud-hypervisor v37.0\n"))
        });
        // The argv-exact discipline: exactly one `--version` probe.
        assert_eq!(runner.invocations().len(), 1);
        assert_eq!(
            runner.invocations()[0].args,
            vec!["--version".to_owned()],
            "the probe runs <cloud_hypervisor> --version and nothing else"
        );
        match gate.verdict() {
            GateVerdict::Proven { observed } => {
                assert_eq!(*observed, version("37.0").expect("parses"));
            }
            GateVerdict::Refused { reason } => {
                unreachable!("the meeting version must prove the gate: {reason}");
            }
        }
        let checked = gate.check().expect("check");
        assert_eq!(checked, version("37.0").expect("parses"));
    }

    #[test]
    fn a_version_below_the_minimum_is_refused_with_both_versions_named() {
        let (gate, runner) =
            gate(|_program, _args| Some(CommandOutput::success("cloud-hypervisor v36.2\n")));
        assert_eq!(runner.invocations().len(), 1);
        let error = gate.check().expect_err("below minimum is refused");
        assert_eq!(error.code, ApiErrorCode::InvalidState);
        assert!(error.detail.contains("36.2.0"), "{error}");
        assert!(error.detail.contains("37.0.0"), "{error}");
        assert!(error.detail.contains("below"), "{error}");
    }

    #[test]
    fn an_unparseable_output_is_an_unproven_refusal() {
        let (gate, _runner) =
            gate(|_program, _args| Some(CommandOutput::success("cloud-hypervisor\n")));
        let error = gate.check().expect_err("unparseable is refused");
        assert!(error.detail.contains("could not be parsed"), "{error}");
    }

    #[test]
    fn a_banner_on_stderr_only_proves_the_gate() {
        // Some builds print the version banner to stderr: the scan
        // accepts it there (stdout first, stderr as the fallback).
        let (gate, _runner) = gate(|_program, _args| {
            let mut output = CommandOutput::success("loading device model\n");
            output.stderr = "cloud-hypervisor v37.1\n".to_owned();
            Some(output)
        });
        match gate.verdict() {
            GateVerdict::Proven { observed } => {
                assert_eq!(*observed, version("37.1").expect("parses"));
            }
            GateVerdict::Refused { reason } => {
                unreachable!("the stderr banner must prove the gate: {reason}");
            }
        }
    }

    #[test]
    fn a_failing_probe_is_a_refusal_carrying_the_excerpt() {
        let (gate, _runner) =
            gate(|_program, _args| Some(CommandOutput::failure("cloud-hypervisor: no such flag")));
        let error = gate.check().expect_err("a failed probe is refused");
        assert!(error.detail.contains("exited unsuccessfully"), "{error}");
        assert!(error.detail.contains("no such flag"), "{error}");
    }

    #[test]
    fn an_unrunnable_binary_is_a_refusal() {
        let runner = FakeRunner::with_closure(|_program, _args| None);
        let gate = VmmVersionGate::probe(
            Some(PathBuf::from("/nonexistent/cloud-hypervisor").as_path()),
            Some("37.0.0"),
            &runner,
        );
        let error = gate.check().expect_err("an unrunnable binary is refused");
        assert!(error.detail.contains("failed to execute"), "{error}");
    }

    #[test]
    fn nothing_configured_refuses_without_probing() {
        let runner = FakeRunner::with_closure(|_program, _args| None);
        let no_binary = VmmVersionGate::probe(None, Some("37.0.0"), &runner);
        let error = no_binary.check().expect_err("no binary is refused");
        assert!(error.detail.contains("cloud_hypervisor"), "{error}");
        assert_eq!(
            runner.invocations().len(),
            0,
            "nothing is probed when the binary is unconfigured"
        );

        let no_minimum = VmmVersionGate::probe(
            Some(PathBuf::from("/usr/bin/cloud-hypervisor").as_path()),
            None,
            &runner,
        );
        let error = no_minimum.check().expect_err("no minimum is refused");
        assert!(error.detail.contains("minimum_version"), "{error}");
        assert_eq!(
            runner.invocations().len(),
            0,
            "nothing is probed when the minimum is unconfigured"
        );
    }

    #[test]
    fn an_unparseable_configured_minimum_is_refused_not_guessed() {
        let runner = FakeRunner::with_closure(|_program, _args| None);
        let gate = VmmVersionGate::probe(
            Some(PathBuf::from("/usr/bin/cloud-hypervisor").as_path()),
            Some("thirty-seven"),
            &runner,
        );
        let error = gate.check().expect_err("an unparseable minimum is refused");
        assert!(error.detail.contains("thirty-seven"), "{error}");
        assert_eq!(
            runner.invocations().len(),
            0,
            "the probe never runs against an unparseable minimum"
        );
    }
}
