//! `--check-config` and `--version` surface tests (P7-A, ADR-0009):
//! the lib check functions, the checked-in example configs (so the
//! docs and the validator cannot drift apart), and the binaries' own
//! smoke behavior. The bin tests are cheap and hermetic — no server,
//! no journal and no network ever start on these paths.

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use volvisord::config;
use volvisord::version::VERSION;
use volvisord::witness;

/// A repository-root file's path, from this crate's tests.
fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// A scratch config file in a tempdir.
fn temp_config(body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, body).expect("write config");
    (dir, path)
}

/// A minimal valid lvm-provider config body.
fn minimal_lvm_toml() -> &'static str {
    "\
listen = \"127.0.0.1:8787\"\n\
journal_dir = \"/var/lib/volvisor/journal\"\n\
provider = \"lvm\"\n\
lvm_vg_prefix = \"volvisor\"\n\
device_claim_token = \"scoped-destructive-auth\"\n"
}

#[test]
fn the_checked_in_example_config_validates_and_summarizes() {
    let summary =
        config::check_config(&repo_file("examples/volvisor.toml")).expect("example validates");
    assert!(summary.contains("127.0.0.1:8787"), "summary: {summary}");
    assert!(summary.contains("provider drbd"), "summary: {summary}");
    assert!(
        summary.contains("witness http://10.0.0.3:9101"),
        "summary: {summary}"
    );
    assert!(summary.contains("migration disabled"), "summary: {summary}");
    // No secret material ever appears in the summary (SPEC-0002
    // section 9).
    assert!(!summary.contains("secret"), "summary: {summary}");
}

#[test]
fn the_checked_in_witness_example_config_validates_and_summarizes() {
    let summary =
        witness::check_config(&repo_file("examples/witnessd.toml")).expect("example validates");
    assert!(
        summary.contains("listen 10.0.0.3:9101"),
        "summary: {summary}"
    );
    assert!(summary.contains("lease_ttl 60s"), "summary: {summary}");
    assert!(summary.contains("host_credentials 2"), "summary: {summary}");
    assert!(!summary.contains("secret"), "summary: {summary}");
}

#[test]
fn malformed_toml_is_a_typed_config_error() {
    let (_dir, path) = temp_config("this is not toml {{{\n");
    let error = config::check_config(&path).expect_err("malformed config");
    assert!(
        error.to_string().contains("cannot parse"),
        "error names the parse failure: {error}"
    );
    let error = witness::check_config(&path).expect_err("malformed witness config");
    assert!(
        error.to_string().contains("cannot parse"),
        "error names the parse failure: {error}"
    );
}

#[test]
fn unknown_fields_are_typed_config_errors() {
    // deny_unknown_fields: a surprise key is a parse-time refusal,
    // never a silently ignored field.
    let (_dir, path) = temp_config(&format!("{}surprise = 1\n", minimal_lvm_toml()));
    let error = config::check_config(&path).expect_err("unknown field");
    assert!(
        error.to_string().contains("cannot parse"),
        "error names the unknown field: {error}"
    );
    let (_dir, path) = temp_config(
        "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\nmystery = true\n",
    );
    let error = witness::check_config(&path).expect_err("unknown witness field");
    assert!(
        error.to_string().contains("cannot parse"),
        "error names the unknown field: {error}"
    );
}

#[test]
fn invalid_values_are_typed_config_errors() {
    // A missing provider-required field is a validation refusal that
    // names the field.
    let (_dir, path) = temp_config(
        "listen = \"127.0.0.1:8787\"\njournal_dir = \"/j\"\nprovider = \"lvm\"\n\
         lvm_vg_prefix = \"volvisor\"\n",
    );
    let error = config::check_config(&path).expect_err("missing device_claim_token");
    assert!(
        error.to_string().contains("device_claim_token"),
        "error names the field: {error}"
    );
    // A zero lease TTL makes the fence window vacuous: refused.
    let (_dir, path) = temp_config(
        "listen = \"127.0.0.1:9101\"\nstate_dir = \"/tmp/opencode/witness\"\n\
         lease_ttl_secs = 0\n",
    );
    let error = witness::check_config(&path).expect_err("zero lease ttl");
    assert!(
        !error.to_string().is_empty(),
        "a typed refusal is produced: {error}"
    );
}

#[test]
fn missing_files_are_typed_config_errors() {
    let missing = repo_file("examples/does-not-exist.toml");
    let error = config::check_config(&missing).expect_err("missing file");
    assert!(
        error.to_string().contains("cannot read"),
        "error names the read failure: {error}"
    );
    let error = witness::check_config(&missing).expect_err("missing witness file");
    assert!(
        error.to_string().contains("cannot read"),
        "error names the read failure: {error}"
    );
}

#[test]
fn bin_version_flags_print_the_compiled_stamp_and_exit_zero() {
    for (name, bin) in [
        ("volvisord", env!("CARGO_BIN_EXE_volvisord")),
        ("volvisor-witnessd", env!("CARGO_BIN_EXE_volvisor-witnessd")),
    ] {
        let output = Command::new(bin).arg("--version").output().expect(name);
        assert!(output.status.success(), "{name} --version exits 0");
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        assert!(
            stdout.contains(VERSION),
            "{name} --version prints the compiled stamp {VERSION:?}: {stdout:?}"
        );
        assert!(!stdout.trim().is_empty(), "{name} --version is non-empty");
    }
}

#[test]
fn bin_check_config_flags_exit_zero_on_the_examples() {
    for (name, bin, example) in [
        (
            "volvisord",
            env!("CARGO_BIN_EXE_volvisord"),
            repo_file("examples/volvisor.toml"),
        ),
        (
            "volvisor-witnessd",
            env!("CARGO_BIN_EXE_volvisor-witnessd"),
            repo_file("examples/witnessd.toml"),
        ),
    ] {
        let output = Command::new(bin)
            .arg("--check-config")
            .arg("--config")
            .arg(&example)
            .output()
            .expect(name);
        assert!(
            output.status.success(),
            "{name} --check-config exits 0 on the checked-in example"
        );
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        assert!(
            stdout.contains("configuration check passed"),
            "{name} logs the summary: {stdout:?}"
        );
    }
}

#[test]
fn bin_check_config_flags_exit_nonzero_on_invalid_configs() {
    let (_dir, path) = temp_config("this is not toml {{{\n");
    for (name, bin) in [
        ("volvisord", env!("CARGO_BIN_EXE_volvisord")),
        ("volvisor-witnessd", env!("CARGO_BIN_EXE_volvisor-witnessd")),
    ] {
        let output = Command::new(bin)
            .arg("--check-config")
            .arg("--config")
            .arg(&path)
            .output()
            .expect(name);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name} --check-config exits non-zero on an invalid config"
        );
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        assert!(
            stderr.contains("configuration check failed"),
            "{name} prints the typed error to stderr: {stderr:?}"
        );
    }
}
