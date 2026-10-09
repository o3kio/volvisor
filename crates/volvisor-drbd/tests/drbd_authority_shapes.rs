//! Shape tests for the DRBD writer-authority command forms the P4a
//! plan adds (docs/plans/2026-10-09-p4-witness-fencing-authority.md
//! §4): `drbdsetup suspend-io`/`resume-io` (the fail-closed
//! self-fencing data-path freeze, addressed by MINOR) and
//! `drbdsetup show-gi` (the per-volume data-generation identity set a
//! witness registration attests as the volume lineage).
//!
//! Every argv form and every output byte asserted here was verified
//! against the drbd-utils 9.29.0 sources (`/tmp/opencode/du-9290`,
//! citations inline) before the fake implements it — the P2/P3
//! lesson. The fixtures drive the fake world through the same
//! closure-mode [`FakeRunner`] dispatch every other command answers
//! through, so the future provider integration will parse exactly
//! what these tests pin.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Fixture, GiSet, SEED_MINOR, fixture, seed_volume};
use volvisor_drbd::provider::resource_name_for;
use volvisor_drbd::report::{Role, parse_drbdsetup_status};
use volvisor_drbd::resgen::res_file_path;
use volvisor_drbd::{CommandOutput, CommandRunner};
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    CreateVolumeRequest, ReplicationModeRequest, ReplicationPolicyRequest,
};
use volvisor_types::{OperationId, ProjectId, VolumeId};

/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;

/// Run one `drbdsetup` command through the fixture's scripted runner
/// (the fake's existing dispatch path).
fn drbdsetup(fixture: &Fixture, args: &[&str]) -> CommandOutput {
    fixture
        .runner
        .run("drbdsetup", args)
        .expect("drbdsetup runs")
}

/// The resource name of a fixture volume id.
fn resource_of(volume_id: &str) -> String {
    resource_name_for(&VolumeId::new(volume_id).expect("valid volume id"))
}

/// A minimal valid nearline create request (mirrors the behavior
/// tests' helper; the file is deliberately independent).
fn nearline_create(volume_id: &str, size_bytes: u64) -> CreateVolumeRequest {
    CreateVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-create-{volume_id}")).expect("valid id"),
        project_id: ProjectId::new("authority-shapes").expect("valid id"),
        volume_id: VolumeId::new(volume_id).expect("valid id"),
        volume_class: VolumeClass::NearlineReplicated,
        size_bytes,
        logical_block_size: None,
        provisioning: None,
        placement: None,
        local_protection: None,
        replication: Some(ReplicationPolicyRequest {
            engine: Some("drbd9".to_owned()),
            mode: ReplicationModeRequest::Async,
            remote_replicas: 1,
            allow_degraded_create: false,
        }),
        migration_policy: None,
        encryption: None,
    }
}

/// `drbdsetup suspend-io`/`resume-io` are CTX_MINOR commands
/// (user/v9/drbdsetup.c:363-366): the argument is a minor number or
/// `/dev/drbd<minor>` (dt_minor_of_dev, user/shared/shared_tool.c:631-674),
/// and a suspended resource reports the verified `suspended:user`
/// qualifier on its status resource line (resource_status,
/// drbdsetup.c:3070-3076; susp_str, 2523-2550).
#[test]
fn suspend_io_freezes_the_data_path_and_status_reports_the_verified_user_reason() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-auth", GIB);
    let resource = resource_of("vol-auth");
    // resource_name_for("vol-auth") — pinned so the verbatim fixtures
    // below cannot silently drift with the naming scheme, and the
    // seed minor ties the fixture to [`SEED_MINOR`].
    assert_eq!(resource, "vol-vol-auth-c47c6a15");
    assert_eq!(SEED_MINOR, 11);

    // Before: the unsuspended verbatim status shape.
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(status.success);
    assert_eq!(
        status.stdout,
        "vol-vol-auth-c47c6a15 role:Secondary\n  \
         disk:UpToDate open:no\n  \
         node-b role:Secondary\n    \
         peer-disk:UpToDate\n\n"
    );

    // suspend-io by bare minor — dt_minor_of_dev spelling 1
    // (shared_tool.c:638-643: "only digits are given").
    let out = drbdsetup(&fixture, &["suspend-io", "11"]);
    assert!(out.success);
    assert_eq!(out.stdout, "");

    // The verified status shape while suspended: ` suspended:user`
    // follows `role:` on the resource line; the device and peer lines
    // are unchanged (the qualifier is the ONLY non-verbose status
    // effect of suspend-io — `al-suspended:` is statistics/verbose
    // only, drbdsetup.c:2591-2612).
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(status.success);
    assert_eq!(
        status.stdout,
        "vol-vol-auth-c47c6a15 role:Secondary suspended:user\n  \
         disk:UpToDate open:no\n  \
         node-b role:Secondary\n    \
         peer-disk:UpToDate\n\n"
    );
    // The provider-side parser reads the qualifier back (its
    // `suspended` vocabulary is the verified susp_str set).
    let parsed = parse_drbdsetup_status(&status.stdout).expect("parse suspended status");
    assert_eq!(parsed.suspended.as_deref(), Some("user"));
    assert_eq!(parsed.role, Role::Secondary);
    assert_eq!(parsed.local_open, Some(false));
    // Wrap budget: the resource line stays within wrap_printf's
    // 80-column budget (user/shared/wrap_printf.c:15-33) — the
    // `suspended:user` qualifier does not wrap on standard vol-...
    // names (names up to 50 columns fit; this one is 21).
    let resource_line = status.stdout.lines().next().expect("resource line");
    assert!(resource_line.chars().count() <= 80);

    // resume-io by device node — dt_minor_of_dev spelling 2
    // (shared_tool.c:651-654: "/dev/drbd" + only digits).
    let out = drbdsetup(&fixture, &["resume-io", "/dev/drbd11"]);
    assert!(out.success);
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(status.success);
    assert_eq!(
        status.stdout,
        "vol-vol-auth-c47c6a15 role:Secondary\n  \
         disk:UpToDate open:no\n  \
         node-b role:Secondary\n    \
         peer-disk:UpToDate\n\n"
    );
    let parsed = parse_drbdsetup_status(&status.stdout).expect("parse resumed status");
    assert_eq!(parsed.suspended, None);
}

/// The minor-only argument discipline: a bare RESOURCE NAME is not
/// resolvable by drbdsetup (dt_minor_of_dev returns -1 for anything
/// that is not digits, `/dev/drbd<digits>` or an existing DRBD-major
/// block node — shared_tool.c:631-674; the P4a plan's verified argv
/// note), `all` is refused outright for a minor-context command
/// (drbdsetup.c:4691-4693), and arity errors follow the generic
/// context-loop shapes (drbdsetup.c:4683-4686, 1150-1156).
#[test]
fn suspend_io_rejects_every_non_minor_argument_form() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-auth", GIB);
    let resource = resource_of("vol-auth");

    // A bare resource name (and any other non-minor spelling) is not
    // resolvable: "Cannot determine minor device number of device
    // '<arg>'" (drbdsetup.c:4697-4701).
    for bad in [&resource, "/dev/drbdminor", "drbd11"] {
        let out = drbdsetup(&fixture, &["suspend-io", bad]);
        assert!(!out.success, "{bad:?} must not resolve to a minor");
        assert_eq!(
            out.stderr,
            format!("Cannot determine minor device number of device '{bad}'")
        );
    }

    // "all" is a CTX_ALL argument; a CTX_MINOR-only command refuses
    // it (drbdsetup.c:4691-4693).
    let out = drbdsetup(&fixture, &["suspend-io", "all"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "command does not accept argument 'all'");

    // Missing argument (drbdsetup.c:4683-4686) and excess arguments
    // (drbdsetup.c:1150-1156 → warn_print_excess_args, 1022-1028).
    let out = drbdsetup(&fixture, &["suspend-io"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "Missing argument 2 to command\n");
    let out = drbdsetup(&fixture, &["suspend-io", "11", "and-more"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "Excess arguments: and-more");

    // None of the refusals suspended anything.
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(status.success);
    assert!(!status.stdout.contains("suspended"));
}

/// A minor with no running resource answers through the netlink error
/// surface: "<obj>: Failure: (127) Device minor not allocated"
/// (check_error, drbdsetup.c:988-991; ERR_MINOR_INVALID message,
/// drbdsetup.c:512). ASSUMPTION(unverified): whether the kernel picks
/// 127 or ERR_RES_NOT_KNOWN (158) for an unknown minor is kernel-side
/// and not in the drbd-utils tree — the fake models 127.
#[test]
fn suspend_io_on_an_unallocated_minor_fails_like_a_kernel_error_reply() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-auth", GIB);
    for verb in ["suspend-io", "resume-io"] {
        let out = drbdsetup(&fixture, &[verb, "19"]);
        assert!(!out.success);
        assert_eq!(out.stderr, "19: Failure: (127) Device minor not allocated");
    }
}

/// Idempotency: drbdsetup ignores SS_NOTHING_TO_DO-class replies
/// (drbdsetup.c:996-997), so a double suspend and a resume of a
/// non-suspended device both succeed and never double-print the
/// qualifier. (ASSUMPTION(unverified): that the kernel classifies the
/// no-op case there — kernel-side.)
#[test]
fn suspend_io_and_resume_io_are_idempotent() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-auth", GIB);
    let resource = resource_of("vol-auth");

    assert!(drbdsetup(&fixture, &["suspend-io", "11"]).success);
    assert!(drbdsetup(&fixture, &["suspend-io", "11"]).success);
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert_eq!(
        status.stdout.matches("suspended:user").count(),
        1,
        "the qualifier appears exactly once"
    );

    assert!(drbdsetup(&fixture, &["resume-io", "11"]).success);
    assert!(drbdsetup(&fixture, &["resume-io", "11"]).success);
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(!status.stdout.contains("suspended"));
}

/// `drbdsetup show-gi` prints the data-generation identity set in the
/// VERBATIM `dt_pretty_print_v9_uuids` shape (user/v9/drbdtool_common.c:89-114):
/// the ASCII-art UUID header, the `dt_print_v9_uuids` line
/// (drbdtool_common.c:64-87 — current:bitmap:history:history in
/// UPPERCASE %016lX hex, drbd_endian.h:156/163, then 7 local and 5
/// peer metadata-flag digits) and the flag legend. The argv is the
/// CTX_PEER_DEVICE triple resource/peer-node-id/volume
/// (drbdsetup.c:388, drbdsetup.h:53-54, parsed at 4736-4739).
#[test]
fn show_gi_prints_the_verified_data_generation_shape() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-gi", GIB);
    let resource = resource_of("vol-gi");
    assert_eq!(resource, "vol-vol-gi-0457c7cb");

    let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
    assert!(out.success);

    // The verbatim fixture: header (drbdtool_common.c:92-97), the
    // UUID line with the resource's deterministic current UUID over
    // the create-md-initialized bitmap/history (all zero,
    // drbdmeta.c:2683-2685) and the create-md flag digits
    // (MDF_AL_CLEAN, drbdmeta.c:2686 — ASSUMPTION(unverified) that a
    // live clean volume reports exactly these kernel-computed flag
    // digits), then the legend (drbdtool_common.c:99-113).
    let expected = concat!(
        "\n",
        "       +--<  Current data generation UUID  >-\n",
        "       |               +--<  Bitmap's base data generation UUID  >-\n",
        "       |               |                 +--<  younger history UUID  >-\n",
        "       |               |                 |         +-<  older history  >-\n",
        "       V               V                 V         V\n",
        "4B3BDA92B09E4EC7:0000000000000000:0000000000000000:0000000000000000:0:0:0:0:1:0:0:0:0:0:0:0\n",
        "                                                                    ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^\n",
        "                                      -<  Data consistency flag  >--+ | | | | | | | | | | |\n",
        "                             -<  Data was/is currently up-to-date  >--+ | | | | | | | | | |\n",
        "                                  -<  Node was/is currently primary  >--+ | | | | | | | | |\n",
        " -<  This node was a crashed primary, and has not seen its peer since  >--+ | | | | | | | |\n",
        "             -<  The activity-log was applied, the disk can be attached  >--+ | | | | | |\n",
        "        -<  The activity-log was disabled, peer is completely out of sync  >--+ | | | | |\n",
        "                              -<  This node was primary when it lost quorum  >--+ | | | |\n",
        "                                          -<  Node was/is currently connected  >--+ | | |\n",
        "                              -<  The peer's disk was out-dated or inconsistent  >--+ | | |\n",
        "                                 -<   A fence policy other the dont-care was used  >--+ | |\n",
        "                  -<  Node was in the progress of marking all blocks as out of sync  >--+ |\n",
        "                     -<  At least once we saw this node with a backing device attached >--+\n",
        "\n",
    );
    assert_eq!(out.stdout, expected);
    // The fake's derivation reproduces the pinned fixture byte for
    // byte (the determinism adoption verification relies on).
    assert_eq!(out.stdout, GiSet::for_resource(&resource).show_gi_text());
}

/// `show_or_get_gi_cmd` walks the KERNEL's peer devices
/// (drbdsetup.c:4153-4165): a resource that is not up answers
/// "<resource>: No such peer device" (exit 10). The identity set
/// itself is on-LV metadata and survives down/up unchanged.
#[test]
fn show_gi_requires_an_up_resource_and_the_lineage_survives_down_up() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-gi", GIB);
    let resource = resource_of("vol-gi");
    let res_file = res_file_path(&fixture.base.join("drbd.d"), &resource);
    let res_file = res_file.to_str().expect("fixture path is UTF-8");

    let before = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
    assert!(before.success);

    // The suspension is runtime kernel state (not metadata): suspend
    // while up, then down — the next up must come back unsuspended.
    assert!(drbdsetup(&fixture, &["suspend-io", "11"]).success);
    let down = fixture
        .runner
        .run("drbdadm", &["-c", res_file, "down", &resource])
        .expect("drbdadm down runs");
    assert!(down.success);
    let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
    assert!(!out.success);
    assert_eq!(out.stderr, format!("{resource}: No such peer device"));

    let up = fixture
        .runner
        .run("drbdadm", &["-c", res_file, "up", &resource])
        .expect("drbdadm up runs");
    assert!(up.success);
    let status = drbdsetup(&fixture, &["status", &resource]);
    assert!(status.success);
    assert!(!status.stdout.contains("suspended"));

    // The identity set is on-LV metadata: byte-identical across the
    // down/up cycle.
    let after = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
    assert!(after.success);
    assert_eq!(after.stdout, before.stdout);
}

/// The peer-device context match (drbdsetup.c:4138-4144) compares
/// resource name, peer node id AND volume; a mismatch answers "No
/// such peer device" (4164-4165). Non-numeric context arguments fail
/// m_strtoll's "not a valid number" (drbdsetup.c:4736-4739 →
/// shared_tool.c:546-547), and arity errors follow the generic
/// context-loop shapes (drbdsetup.c:4683-4686, 1150-1156).
#[test]
fn show_gi_rejects_context_mismatches_and_malformed_arguments() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vol-gi", GIB);
    let resource = resource_of("vol-gi");

    // Wrong volume (the fixture resources are single-volume) and the
    // wrong peer node id (the fixture peer is node-id 1) match no
    // peer device.
    for args in [
        vec!["show-gi", &resource, "1", "1"],
        vec!["show-gi", &resource, "0", "0"],
        vec!["show-gi", &resource, "7", "0"],
    ] {
        let out = drbdsetup(&fixture, &args);
        assert!(!out.success, "{args:?} must not match a peer device");
        assert_eq!(out.stderr, format!("{resource}: No such peer device"));
    }

    // An unknown resource is the same refusal.
    let out = drbdsetup(&fixture, &["show-gi", "vol-unknown-00000000", "1", "0"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "vol-unknown-00000000: No such peer device");

    // Non-numeric peer node id / volume.
    let out = drbdsetup(&fixture, &["show-gi", &resource, "one", "0"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "one is not a valid number");
    let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "zero"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "zero is not a valid number");

    // Missing and excess arguments (with resource + peer node id
    // given, the volume argument is the missing one — index 4).
    let out = drbdsetup(&fixture, &["show-gi", &resource, "1"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "Missing argument 4 to command\n");
    let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0", "extra"]);
    assert!(!out.success);
    assert_eq!(out.stderr, "Excess arguments: extra");
}

/// Lineage determinism: re-seeding the same world reproduces the same
/// identity set byte for byte, the provider's create-md flow assigns
/// exactly the seeded set, and a different volume is a different
/// lineage — the properties adoption verification depends on (the
/// UUID set recorded at registration time must match a later reading
/// after a fake restart, and must distinguish volumes).
#[tokio::test]
async fn lineage_uuids_are_deterministic_across_world_reseeds() {
    let mut outputs = Vec::new();
    for _ in 0..2 {
        let fixture = fixture();
        seed_volume(&fixture.base, &fixture.world, "vol-auth", GIB);
        let resource = resource_of("vol-auth");
        let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
        assert!(out.success);
        outputs.push(out.stdout);
    }
    assert_eq!(outputs[0], outputs[1], "re-seeded worlds agree");

    // The provider create path (create-md assigns the set) produces
    // the same bytes as the seeded fixture for the same volume id.
    let created = {
        let fixture = fixture();
        fixture
            .provider
            .create_volume(&nearline_create("vol-auth", GIB))
            .await
            .expect("create volume");
        let resource = resource_of("vol-auth");
        let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
        assert!(out.success);
        out.stdout
    };
    assert_eq!(created, outputs[0], "create-md and seed agree");

    // A different volume is a different lineage.
    let other = {
        let fixture = fixture();
        seed_volume(&fixture.base, &fixture.world, "vol-auth-2", GIB);
        let resource = resource_of("vol-auth-2");
        let out = drbdsetup(&fixture, &["show-gi", &resource, "1", "0"]);
        assert!(out.success);
        out.stdout
    };
    assert_ne!(other, outputs[0]);
}
