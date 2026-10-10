//! Cloud Hypervisor adapter tests (P4b plan §5): [`ChRemoteVmm`]
//! over a [`FakeRunner`], argv-exactly — the same discipline the DRBD
//! provider tests its commands with. Every test scripts the
//! `ch-remote` outputs (the verified surface: `pause`, `info`,
//! `snapshot file://DIR`, `delete`, `restore source_url=file://DIR`,
//! `resume`) and asserts the exact argv the adapter issued, including
//! the `--api-socket` convention (`{api_socket_dir}/{vm_id}.sock`)
//! and the `file://` URL spelling.
//!
//! The restore tests exercise the `config.json` disk-path rewrite
//! against **real files** (the adapter does real filesystem I/O
//! there): matching paths are a verified no-op (the file is not
//! touched), divergent paths are rewritten and re-read-confirmed, and
//! a malformed config or a mapping without a config disk is a typed
//! refusal with no `restore` issued.
//!
//! [`FakeVmm`] is a different double (an in-memory world, not a
//! scripted runner) and is unit-tested in the module; the
//! fake-VMM↔fake-DRBD interplay is the end-to-end stage-B2 matrix's
//! business (later slices).
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use volvisor_provider::{
    ChRemoteConfig, ChRemoteVmm, CommandOutput, CommandRunner, DiskMapping, FakeRunner, Invocation,
    VmState, VmmController,
};
use volvisor_types::ApiErrorCode;

/// The configured API socket directory (the plan §6 convention).
const SOCKET_DIR: &str = "/run/volvisor/vms";

/// The adapter under a scripted runner, plus the runner for
/// invocation assertions.
fn adapter(
    script: impl Fn(&str) -> Option<CommandOutput> + Send + Sync + 'static,
) -> (ChRemoteVmm, Arc<FakeRunner>) {
    let runner = Arc::new(FakeRunner::with_closure(move |_program, args| {
        // argv: ["--api-socket", SOCK, command, ...] — the script
        // keys on the command.
        let command = args.get(2).copied()?;
        script(command)
    }));
    let vmm = ChRemoteVmm::new(config(), Arc::clone(&runner) as Arc<dyn CommandRunner>);
    (vmm, runner)
}

/// The adapter config pinned to a fake `ch-remote` binary name.
fn config() -> ChRemoteConfig {
    ChRemoteConfig {
        ch_remote_bin: PathBuf::from("ch-remote"),
        api_socket_dir: PathBuf::from(SOCKET_DIR),
    }
}

/// The socket path the convention assigns to one VM.
fn socket(vm_id: &str) -> String {
    format!("{SOCKET_DIR}/{vm_id}.sock")
}

/// The exact argv one invocation must carry.
fn argv(vm_id: &str, tail: &[&str]) -> Vec<String> {
    let mut argv = vec!["--api-socket".to_owned(), socket(vm_id)];
    argv.extend(tail.iter().map(|arg| (*arg).to_owned()));
    argv
}

/// Assert one recorded invocation is exactly `ch-remote --api-socket
/// S <tail>`.
fn assert_invocation(invocation: &Invocation, vm_id: &str, tail: &[&str]) {
    assert_eq!(invocation.program, "ch-remote");
    assert_eq!(invocation.args, argv(vm_id, tail));
}

/// A successful `vm.info` output reporting `state`.
fn info(state: &str) -> CommandOutput {
    CommandOutput::success(format!(
        r#"{{"config": {{"cpus": 4}}, "state": "{state}", "device_tree": {{}}}}"#
    ))
}

/// The not-found-shaped `vm.info` failure (the only shape the adapter
/// maps to `Absent` — see the module docs' matching rule).
fn not_found() -> CommandOutput {
    CommandOutput::failure("Error running command \"VmInfo\": 404 Not Found: VM not found")
}

/// One disk mapping.
fn mapping(declared: &str, device: &str) -> DiskMapping {
    DiskMapping {
        declared_path: declared.to_owned(),
        device_path: device.to_owned(),
    }
}

/// Write a snapshot config into `dir` and return the bytes written.
fn write_config(dir: &std::path::Path, json: &str) -> String {
    fs::create_dir_all(dir).expect("create dir");
    fs::write(dir.join("config.json"), json).expect("write config");
    json.to_owned()
}

#[test]
fn socket_path_follows_the_configured_convention() {
    let (vmm, _runner) = adapter(|_| None);
    assert_eq!(
        vmm.socket_path("vm-1"),
        PathBuf::from("/run/volvisor/vms/vm-1.sock")
    );
}

#[test]
fn pause_runs_pause_then_a_verified_info_and_returns_the_proof() {
    let (vmm, runner) = adapter(|command| match command {
        "pause" => Some(CommandOutput::success("")),
        "info" => Some(info("Paused")),
        _ => None,
    });
    let proof = vmm.pause("vm-1").expect("pause");
    assert_eq!(proof.vm_id, "vm-1");
    assert_eq!(proof.state, VmState::Paused);
    assert!(proof.observed_at > 0);

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    assert_invocation(&invocations[0], "vm-1", &["pause"]);
    assert_invocation(&invocations[1], "vm-1", &["info"]);
}

#[test]
fn pause_is_refused_typed_when_info_does_not_confirm_paused() {
    let (vmm, runner) = adapter(|command| match command {
        "pause" => Some(CommandOutput::success("")),
        "info" => Some(info("Running")),
        _ => None,
    });
    let err = vmm
        .pause("vm-1")
        .expect_err("an unverified pause is never a proof");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("not verified"), "{err}");
    assert!(err.detail.contains("Running"), "{err}");
    // Both commands ran: the pause, then the verification info.
    assert_eq!(runner.invocations().len(), 2);
}

#[test]
fn pause_command_failure_maps_to_the_typed_booted_precondition_refusal() {
    let (vmm, runner) = adapter(|command| match command {
        "pause" => Some(CommandOutput::failure(
            "Error running command \"Pause\": Vm not booted",
        )),
        _ => None,
    });
    let err = vmm
        .pause("vm-1")
        .expect_err("a not-booted VM is a typed refusal");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(
        err.detail.contains("/vm.pause requires the VM booted"),
        "{err}"
    );
    assert!(err.detail.contains("Vm not booted"), "{err}");
    assert_eq!(runner.invocations().len(), 1);
}

#[test]
fn pause_command_not_found_failure_maps_to_not_found() {
    let (vmm, _runner) = adapter(|command| match command {
        "pause" => Some(not_found()),
        _ => None,
    });
    let err = vmm.pause("vm-1").expect_err("an absent VM is NOT_FOUND");
    assert_eq!(err.code, ApiErrorCode::NotFound);
    assert!(err.detail.contains("vm-1"), "{err}");
}

#[test]
fn state_parses_each_verified_vm_info_state() {
    // The verified vm.info states; Shutoff maps to Created (a
    // defined, not-running VM — the module docs' recorded choice).
    for (reported, expected) in [
        ("Created", VmState::Created),
        ("Running", VmState::Running),
        ("Paused", VmState::Paused),
        ("Shutoff", VmState::Created),
    ] {
        let (vmm, runner) = adapter(|command| (command == "info").then(|| info(reported)));
        assert_eq!(
            vmm.state("vm-1").expect("state"),
            expected,
            "vm.info reporting {reported}"
        );
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1);
        assert_invocation(&invocations[0], "vm-1", &["info"]);
    }
}

#[test]
fn state_maps_the_not_found_shape_to_absent() {
    let (vmm, runner) = adapter(|command| (command == "info").then(not_found));
    assert_eq!(vmm.state("vm-1").expect("state"), VmState::Absent);
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_invocation(&invocations[0], "vm-1", &["info"]);
}

#[test]
fn state_surfaces_unrecognized_failures_typed_never_absent() {
    // A dead VMM socket (or any failure outside the not-found shape)
    // must NOT read as "no VM": it surfaces typed — the plan §3
    // IN_DOUBT stall, never a phantom-absent no-op.
    let (vmm, _runner) = adapter(|command| {
        (command == "info").then(|| {
            CommandOutput::failure(
                "Error running command \"ApiRequest\": failed to connect to Unix socket",
            )
        })
    });
    let err = vmm
        .state("vm-1")
        .expect_err("an unreachable socket is typed");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("failed to connect"), "{err}");
}

#[test]
fn state_refuses_malformed_or_unknown_info_output_typed() {
    for output in [
        CommandOutput::success("not json at all"),
        CommandOutput::success("{\"config\": {}}"),
        CommandOutput::success("{\"state\": 7}"),
        CommandOutput::success("{\"state\": \"Zombie\"}"),
    ] {
        let (vmm, _runner) = adapter(move |command| (command == "info").then(|| output.clone()));
        let err = vmm
            .state("vm-1")
            .expect_err("unparseable info is never a guess");
        assert_eq!(err.code, ApiErrorCode::Internal, "{err}");
    }
}

#[test]
fn snapshot_argv_is_exact_with_the_file_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (vmm, runner) =
        adapter(|command| (command == "snapshot").then(|| CommandOutput::success("")));
    vmm.snapshot("vm-1", dir.path()).expect("snapshot");
    let url = format!("file://{}", dir.path().display());
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_invocation(&invocations[0], "vm-1", &["snapshot", &url]);
}

#[test]
fn snapshot_failure_maps_typed_with_the_paused_precondition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (vmm, _runner) = adapter(|command| {
        (command == "snapshot")
            .then(|| CommandOutput::failure("Error running command \"Snapshot\": Vm not paused"))
    });
    // The adapter relies on the coordinator's ordering and does not
    // re-check paused itself — cloud-hypervisor refuses an unpaused
    // snapshot and the refusal surfaces typed here (recorded, not
    // papered over).
    let err = vmm.snapshot("vm-1", dir.path()).expect_err("typed refusal");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(
        err.detail.contains("/vm.snapshot requires the VM paused"),
        "{err}"
    );
}

#[test]
fn destroy_of_an_absent_vm_issues_no_delete() {
    // The crash-reconcile dependency: re-driving destroy of an
    // already-absent VM succeeds WITHOUT issuing `delete`.
    let (vmm, runner) = adapter(|command| (command == "info").then(not_found));
    vmm.destroy("vm-1")
        .expect("destroy of an absent VM is a no-op");
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert_invocation(&invocations[0], "vm-1", &["info"]);
}

#[test]
fn destroy_of_a_present_vm_runs_delete() {
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(info("Paused")),
        "delete" => Some(CommandOutput::success("")),
        _ => None,
    });
    vmm.destroy("vm-1").expect("destroy");
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    assert_invocation(&invocations[0], "vm-1", &["info"]);
    assert_invocation(&invocations[1], "vm-1", &["delete"]);
}

#[test]
fn resume_argv_is_exact() {
    let (vmm, runner) =
        adapter(|command| (command == "resume").then(|| CommandOutput::success("")));
    vmm.resume("vm-1").expect("resume");
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_invocation(&invocations[0], "vm-1", &["resume"]);
}

#[test]
fn restore_refuses_a_non_empty_vmm_without_running_restore() {
    let (vmm, runner) = adapter(|command| (command == "info").then(|| info("Paused")));
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"disks": [{"path": "/dev/drbd1"}]}"#);
    let err = vmm
        .restore("vm-1", dir.path(), &[mapping("/dev/drbd1", "/dev/drbd2")])
        .expect_err("restore refuses a non-empty VMM typed");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("non-empty"), "{err}");
    assert!(err.detail.contains("Paused"), "{err}");
    // The refusal happened at the observation: no restore command, no
    // config rewrite.
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert_invocation(&invocations[0], "vm-1", &["info"]);
    assert_eq!(
        fs::read_to_string(dir.path().join("config.json")).expect("config"),
        r#"{"disks": [{"path": "/dev/drbd1"}]}"#
    );
}

#[test]
fn restore_rewrites_divergent_disk_paths_and_verifies() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(
        dir.path(),
        r#"{"cpus": 4, "disks": [{"path": "/dev/drbd1"}, {"path": "/dev/drbd2"}]}"#,
    );
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    vmm.restore(
        "vm-1",
        dir.path(),
        &[
            mapping("/dev/drbd1", "/dev/drbd-by-res/vol-1"),
            mapping("/dev/drbd2", "/dev/drbd-by-res/vol-2"),
        ],
    )
    .expect("restore");

    // The rewrite landed on disk and the unrelated fields survived.
    let rewritten: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("config.json")).expect("config"))
            .expect("valid json");
    assert_eq!(rewritten["disks"][0]["path"], "/dev/drbd-by-res/vol-1");
    assert_eq!(rewritten["disks"][1]["path"], "/dev/drbd-by-res/vol-2");
    assert_eq!(rewritten["cpus"], 4);

    let url = format!("file://{}", dir.path().display());
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    assert_invocation(&invocations[0], "vm-1", &["info"]);
    assert_invocation(
        &invocations[1],
        "vm-1",
        &["restore", &format!("source_url={url}")],
    );
}

#[test]
fn restore_is_a_verified_no_op_when_paths_match() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Symmetric minors (the common DRBD case): the rewrite must not
    // touch the file at all.
    let original = write_config(
        dir.path(),
        r#"{"cpus": 4, "disks": [{"path": "/dev/drbd1"}, {"path": "/dev/drbd2"}]}"#,
    );
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    vmm.restore(
        "vm-1",
        dir.path(),
        &[
            mapping("/dev/drbd1", "/dev/drbd1"),
            mapping("/dev/drbd2", "/dev/drbd2"),
        ],
    )
    .expect("restore");
    assert_eq!(
        fs::read_to_string(dir.path().join("config.json")).expect("config"),
        original,
        "a matching mapping must leave the file byte-identical"
    );
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 2);
    let url = format!("file://{}", dir.path().display());
    assert_invocation(
        &invocations[1],
        "vm-1",
        &["restore", &format!("source_url={url}")],
    );
}

#[test]
fn restore_refuses_a_missing_config_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    let err = vmm
        .restore("vm-1", dir.path(), &[])
        .expect_err("a missing snapshot config fails typed");
    assert_eq!(err.code, ApiErrorCode::NotFound);
    assert!(err.detail.contains("config.json"), "{err}");
    // No restore command was issued.
    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
}

#[test]
fn restore_refuses_a_malformed_config_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), "not json");
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    let err = vmm
        .restore("vm-1", dir.path(), &[])
        .expect_err("a malformed config is never a guess");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("not valid JSON"), "{err}");
    assert_eq!(runner.invocations().len(), 1);
}

#[test]
fn restore_refuses_an_unexpected_config_structure_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"cpus": 4}"#);
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    let err = vmm
        .restore("vm-1", dir.path(), &[])
        .expect_err("a config without a disks array is a typed refusal");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(
        err.detail.contains("unexpected snapshot config structure"),
        "{err}"
    );
    assert_eq!(runner.invocations().len(), 1);
}

#[test]
fn restore_refuses_a_mapping_without_a_config_disk_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"disks": [{"path": "/dev/drbd1"}]}"#);
    let (vmm, runner) = adapter(|command| match command {
        "info" => Some(not_found()),
        "restore" => Some(CommandOutput::success("")),
        _ => None,
    });
    // The migration believes the VM has /dev/other; the snapshot
    // disagrees — never a guess, never a silent skip.
    let err = vmm
        .restore("vm-1", dir.path(), &[mapping("/dev/other", "/dev/x")])
        .expect_err("an unmatched mapping is a typed refusal");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("/dev/other"), "{err}");
    assert_eq!(runner.invocations().len(), 1);
    // The config was not rewritten by the failing attempt.
    assert_eq!(
        fs::read_to_string(dir.path().join("config.json")).expect("config"),
        r#"{"disks": [{"path": "/dev/drbd1"}]}"#
    );
}

// ---------------------------------------------------------------------
// resize_disk (P6-B): byte-exact HTTP over a real unix domain socket
// ---------------------------------------------------------------------

/// Build the adapter over `socket_dir` with an inert scripted runner
/// (resize_disk must not touch the runner — it is REST-only).
fn resize_adapter(socket_dir: &std::path::Path) -> ChRemoteVmm {
    ChRemoteVmm::new(
        ChRemoteConfig {
            ch_remote_bin: PathBuf::from("ch-remote"),
            api_socket_dir: socket_dir.to_path_buf(),
        },
        Arc::new(FakeRunner::with_closure(|_program, _args| None)) as Arc<dyn CommandRunner>,
    )
}

/// Run a one-shot fake UDS HTTP server for `vm_id` (the adapter's
/// socket convention), scripting `response`: accept one connection,
/// capture the exact request bytes, answer and close. The P6-B
/// discipline: the argv-exact contract of the `ch-remote` commands
/// translates to byte-exact HTTP assertions for the REST-only
/// resize-disk call (no `ch-remote` subcommand exists — verified
/// against cloud-hypervisor v37.0's published command list).
fn serve_resize_once(
    socket_dir: &std::path::Path,
    vm_id: &str,
    response: &str,
) -> std::thread::JoinHandle<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let socket = socket_dir.join(format!("{vm_id}.sock"));
    let listener = UnixListener::bind(&socket).expect("bind the fake api socket");
    let response = response.to_owned();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one connection");
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).expect("read the request");
            raw.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some(header_end) = text.find("\r\n\r\n") {
                let headers = &text[..header_end];
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        (name.eq_ignore_ascii_case("content-length"))
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or_default();
                if raw.len() >= header_end + 4 + length {
                    break;
                }
            }
            if read == 0 {
                break;
            }
        }
        stream
            .write_all(response.as_bytes())
            .expect("write the scripted response");
        drop(stream);
        String::from_utf8_lossy(&raw).into_owned()
    })
}

#[test]
fn resize_disk_writes_the_exact_http_bytes_and_accepts_204() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = serve_resize_once(dir.path(), "vm-1", "HTTP/1.1 204 No Content\r\n\r\n");
    let vmm = resize_adapter(dir.path());
    vmm.resize_disk("vm-1", "vol-1", 2_147_483_648)
        .expect("204 is the verified success shape");
    let request = server.join().expect("the server thread");
    assert_eq!(
        request,
        "PUT /api/v1/vm.resize-disk HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: 36\r\n\
         Connection: close\r\n\
         \r\n\
         {\"id\":\"vol-1\",\"new_size\":2147483648}",
        "the request bytes are pinned: method, path, headers and the \
         VmResizeDisk body (id + new_size, slot omitted)"
    );
}

#[test]
fn resize_disk_is_rest_only_and_never_touches_the_runner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = serve_resize_once(dir.path(), "vm-1", "HTTP/1.1 204 No Content\r\n\r\n");
    let runner = Arc::new(FakeRunner::with_closure(|_program, _args| None));
    let vmm = ChRemoteVmm::new(
        ChRemoteConfig {
            ch_remote_bin: PathBuf::from("ch-remote"),
            api_socket_dir: dir.path().to_path_buf(),
        },
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
    );
    vmm.resize_disk("vm-1", "vol-1", 2048).expect("resize");
    assert_eq!(
        runner.invocations(),
        Vec::<Invocation>::new(),
        "resize_disk is REST-only: no ch-remote command runs"
    );
    server.join().expect("the server thread");
}

#[test]
fn resize_disk_maps_a_non_2xx_answer_to_a_typed_refusal_carrying_the_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = serve_resize_once(
        dir.path(),
        "vm-1",
        "HTTP/1.1 500 Internal Server Error\r\n\r\n{\"error\":\"Failed to resize disk\"}",
    );
    let vmm = resize_adapter(dir.path());
    let err = vmm
        .resize_disk("vm-1", "vol-1", 2048)
        .expect_err("a VMM refusal is a typed error");
    assert_eq!(err.code, ApiErrorCode::InvalidState);
    assert!(err.detail.contains("500"), "the status is carried: {err}");
    assert!(
        err.detail.contains("vol-1"),
        "the disk id is carried: {err}"
    );
    assert!(err.detail.contains("Failed to resize disk"), "{err}");
    server.join().expect("the server thread");
}

#[test]
fn resize_disk_fails_typed_when_nothing_listens_on_the_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vmm = resize_adapter(dir.path());
    let err = vmm
        .resize_disk("vm-1", "vol-1", 2048)
        .expect_err("a dead socket is a typed transport failure");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("failed to connect"), "{err}");
    let socket = dir.path().join("vm-1.sock").display().to_string();
    assert!(
        err.detail.contains(&socket),
        "the error names the socket it targeted: {err}"
    );
}
