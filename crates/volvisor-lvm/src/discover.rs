//! Read-only physical-device discovery.
//!
//! Discovery never mutates anything: it runs `lsblk --json --bytes` and
//! reads `/dev/disk/by-id` (resolving symlinks) to establish a **stable
//! hardware identity** — WWN first, serial second — for every whole disk.
//! A Linux device name (`/dev/nvme0n1`, `/dev/sda`) or PCI BDF is never
//! used as an identity (SPEC-0002 section 3, AGENTS rule 7); kernel names
//! only contribute entropy to the derived opaque [`DeviceId`].
//!
//! Loop and ram devices are skipped (only `disk`-type block devices are
//! considered), and devices without any stable identity are skipped rather
//! than guessed at. Multi-namespace NVMe (one controller, several
//! namespaces) is out of P0 scope: each namespace surfaces as its own disk
//! with a distinct identity, and namespace grouping is not attempted.
//!
//! Health is reported as `Unknown` — never `Healthy` — until proven by
//! real evidence, and ownership fields are empty: claim state lives in the
//! provider state file, not in discovery output.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use volvisor_types::domain::{Generation, PhysicalDevice};
use volvisor_types::{ApiError, ApiErrorCode, DeviceId, Health, HostId};

use crate::CommandRunner;
use crate::report::LsblkOutput;

/// Host identity stamped on discovered devices.
///
/// Discovery is host-local by construction in the P0 single-host daemon;
/// the daemon layer stamps the real host identity when it forwards
/// discovery results.
const LOCAL_HOST_ID: &str = "local";

/// The `lsblk` invocation used for discovery.
const LSBLK_ARGS: &[&str] = &[
    "--json",
    "--bytes",
    "--output",
    "NAME,TYPE,SIZE,SERIAL,WWN,MODEL",
];

/// A device as seen by discovery, including provider-internal details
/// (kernel path) that the public [`PhysicalDevice`] record deliberately
/// does not expose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// Opaque stable device identity (never a Linux name or BDF).
    pub device_id: DeviceId,
    /// Kernel device name (`sda`, `nvme0n1`, ...); internal only.
    pub kernel_name: String,
    /// Device path used for `pvcreate` (by-id path when available).
    pub path: String,
    /// Capacity in bytes.
    pub size_bytes: u64,
    /// The stable hardware identity string (WWN or serial).
    pub stable_identity: String,
}

/// Discover all whole disks with a stable hardware identity.
///
/// Read-only: no signature scanning beyond reading `lsblk` output and the
/// `/dev/disk/by-id` directory, and no wiping or adoption ever happens
/// here.
pub fn discover_devices(
    runner: &dyn CommandRunner,
    sysfs_root: &Path,
) -> Result<Vec<PhysicalDevice>, ApiError> {
    let devices = scan(runner, sysfs_root)?;
    let mut records = Vec::with_capacity(devices.len());
    for device in &devices {
        records.push(physical_device(device)?);
    }
    Ok(records)
}

/// Scan and return devices including provider-internal kernel paths.
///
/// Used by `claim_device` to resolve a [`DeviceId`] back to a path; not
/// part of the tenant-facing surface.
pub fn scan(
    runner: &dyn CommandRunner,
    sysfs_root: &Path,
) -> Result<Vec<DiscoveredDevice>, ApiError> {
    let output = runner.run("lsblk", LSBLK_ARGS)?;
    if !output.success {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            format!("lsblk failed: {}", output.stderr_excerpt()),
        ));
    }
    let parsed: LsblkOutput = serde_json::from_str(&output.stdout).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("failed to parse lsblk JSON output: {e}"),
        )
    })?;
    let by_id = read_by_id_index(&sysfs_root.join("dev").join("disk").join("by-id"));

    let mut devices = Vec::new();
    for device in parsed.blockdevices {
        let Some(name) = device.name.clone() else {
            continue;
        };
        let device_type = device.device_type.clone().unwrap_or_default();
        // Only whole disks; loop and ram devices are explicitly skipped.
        if device_type != "disk"
            || name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
        {
            continue;
        }
        let Some(size_bytes) = device.size_bytes() else {
            // No honest size -> skip rather than fabricate one.
            continue;
        };
        // Stable identity: WWN first, serial second; a by-id name is the
        // last resort. Without any of these the device is not claimable.
        let stable_identity = non_empty(device.wwn.as_deref())
            .or_else(|| non_empty(device.serial.as_deref()))
            .or_else(|| by_id.get(&name).and_then(|names| names.first().cloned()));
        let Some(stable_identity) = stable_identity else {
            continue;
        };
        let device_id = derive_device_id(&stable_identity, &name, size_bytes)?;
        // Prefer the stable by-id path for pv operations; fall back to the
        // kernel path when udev did not create one.
        let path = by_id
            .get(&name)
            .and_then(|names| names.first())
            .map_or_else(
                || format!("/dev/{name}"),
                |id| format!("/dev/disk/by-id/{id}"),
            );
        devices.push(DiscoveredDevice {
            device_id,
            kernel_name: name,
            path,
            size_bytes,
            stable_identity,
        });
    }
    Ok(devices)
}

/// Build the public domain record for a discovered device.
fn physical_device(device: &DiscoveredDevice) -> Result<PhysicalDevice, ApiError> {
    Ok(PhysicalDevice {
        id: device.device_id.clone(),
        host_id: local_host_id()?,
        // Multi-namespace NVMe grouping is out of P0 scope.
        namespace_ids: Vec::new(),
        capacity_bytes: device.size_bytes,
        // Unknown until proven; never a healthy default.
        health: Health::Unknown,
        owner_role: None,
        owner_generation: Generation(0),
    })
}

/// The built-in host identity used for discovery records.
///
/// The constant is valid by construction; if it ever became invalid the
/// honest failure is an `INTERNAL` error, never a panic.
fn local_host_id() -> Result<HostId, ApiError> {
    HostId::new(LOCAL_HOST_ID).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("invalid built-in local host identity: {e}"),
        )
    })
}

/// Derive the opaque device identity.
///
/// `dev-` + the first 32 hex characters (16 bytes) of
/// SHA-256(stable_identity + kernel_name + size_bytes). The kernel name
/// and size contribute entropy so a recycled serial on differently sized
/// media cannot alias an existing identity, but the identity itself never
/// *is* a device path.
fn derive_device_id(
    stable_identity: &str,
    kernel_name: &str,
    size_bytes: u64,
) -> Result<DeviceId, ApiError> {
    let mut hasher = Sha256::new();
    hasher.update(stable_identity.as_bytes());
    hasher.update(b"|");
    hasher.update(kernel_name.as_bytes());
    hasher.update(b"|");
    hasher.update(size_bytes.to_string().as_bytes());
    let digest = hasher.finalize();
    DeviceId::new(format!("dev-{}", hex_prefix(&digest, 16))).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("derived an invalid device identity: {e}"),
        )
    })
}

/// Lowercase hexadecimal of the first `take` bytes, without `format!`.
pub(crate) fn hex_prefix(bytes: &[u8], take: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(take * 2);
    for byte in bytes.iter().take(take) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Read `/dev/disk/by-id` and index the entries by resolved kernel name.
///
/// Each entry is a symlink (e.g. `wwn-0x5000c5...` -> `../../sda`); the
/// symlink target's file name is the kernel device it refers to. A missing
/// directory yields an empty index (the system may not run udev); lsblk
/// remains the primary source.
fn read_by_id_index(by_id_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut index = BTreeMap::new();
    let Ok(entries) = fs::read_dir(by_id_dir) else {
        return index;
    };
    for entry in entries.flatten() {
        let Some(id_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(kernel_name) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        index
            .entry(kernel_name.to_owned())
            .or_insert_with(Vec::new)
            .push(id_name);
    }
    index
}

/// The string when non-empty (after trimming), else `None`.
fn non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = value.map(str::trim).filter(|v| !v.is_empty());
    trimmed.map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandOutput, FakeRunner};
    use std::path::PathBuf;

    const LSBLK_JSON: &str = r#"{"blockdevices":[
        {"name":"loop0","type":"loop","size":104857600,"serial":null,"wwn":null,"model":null},
        {"name":"ram0","type":"ram","size":65536},
        {"name":"sda","type":"disk","size":1000204886016,
         "serial":"S6PXND0R123456","wwn":"0x5000c50015ead127","model":"Samsung SSD"},
        {"name":"nvme0n1","type":"disk","size":512110190592,
         "serial":"INVME-1234","wwn":"eui.0025388b7102e7b2","model":"NVMe ctrl"},
        {"name":"sdb","type":"disk","size":123456789,
         "serial":"","wwn":"","model":"No identity at all"}
    ]}"#;

    fn fixture_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let by_id = dir.path().join("dev/disk/by-id");
        std::fs::create_dir_all(&by_id).expect("by-id dir");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../../sda", by_id.join("wwn-0x5000c50015ead127"))
                .expect("sda symlink");
            std::os::unix::fs::symlink("../../sdb", by_id.join("scsi-SATA_x"))
                .expect("sdb symlink");
        }
        dir
    }

    fn runner() -> FakeRunner {
        FakeRunner::with_closure(|program, args| {
            (program == "lsblk" && args.contains(&"--json"))
                .then(|| CommandOutput::success(LSBLK_JSON.to_owned()))
        })
    }

    #[test]
    fn discovery_parses_lsblk_and_resolves_by_id() {
        let root = fixture_root();
        let devices = discover_devices(&runner(), root.path()).expect("discover");
        // loop0 and ram0 are skipped; sda, nvme0n1 have WWNs; sdb has no
        // serial/wwn but is rescued by its by-id entry.
        assert_eq!(devices.len(), 3, "loop, ram devices are skipped");

        let sda = devices
            .iter()
            .find(|d| d.capacity_bytes == 1_000_204_886_016)
            .expect("sda");
        // Identity is the opaque derived id, never a bare device path.
        assert!(sda.id.as_str().starts_with("dev-"));
        assert_eq!(sda.id.as_str().len(), "dev-".len() + 32);
        assert!(!sda.id.as_str().contains('/'));
        assert_eq!(sda.host_id.as_str(), "local");
        assert_eq!(sda.health, Health::Unknown);
        assert_eq!(sda.owner_role, None);
        assert_eq!(sda.owner_generation, Generation(0));
        assert_eq!(sda.namespace_ids, Vec::<String>::new());

        // The by-id path was resolved for pv operations (internal detail).
        let scanned = scan(&runner(), root.path()).expect("scan");
        let sda_scan = scanned
            .iter()
            .find(|d| d.kernel_name == "sda")
            .expect("sda scan");
        assert_eq!(sda_scan.path, "/dev/disk/by-id/wwn-0x5000c50015ead127");
        assert_eq!(sda_scan.stable_identity, "0x5000c50015ead127");

        // WWN takes precedence over serial; serial is the fallback.
        let nvme = scanned
            .iter()
            .find(|d| d.kernel_name == "nvme0n1")
            .expect("nvme scan");
        assert_eq!(nvme.stable_identity, "eui.0025388b7102e7b2");

        // A disk without serial/wwn falls back to its by-id name.
        let sdb = scanned
            .iter()
            .find(|d| d.kernel_name == "sdb")
            .expect("sdb scan");
        assert_eq!(sdb.stable_identity, "scsi-SATA_x");

        // Distinct devices derive distinct identities.
        assert_ne!(sda_scan.device_id, nvme.device_id);
    }

    #[test]
    fn identity_is_deterministic_and_sensitive_to_inputs() {
        let a = derive_device_id("wwn-x", "sda", 100).expect("id a");
        let b = derive_device_id("wwn-x", "sda", 100).expect("id b");
        let c = derive_device_id("wwn-x", "sdb", 100).expect("id c");
        let d = derive_device_id("wwn-y", "sda", 100).expect("id d");
        assert_eq!(a, b);
        assert_ne!(a, c, "kernel name contributes entropy");
        assert_ne!(a, d, "stable identity contributes entropy");
    }

    #[test]
    fn missing_by_id_directory_is_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let devices = discover_devices(&runner(), dir.path()).expect("discover");
        // Without by-id, sdb has no stable identity at all and is skipped.
        assert_eq!(devices.len(), 2, "lsblk identity works without by-id");
        let scanned = scan(&runner(), dir.path()).expect("scan");
        let sda = scanned
            .iter()
            .find(|d| d.kernel_name == "sda")
            .expect("sda");
        assert_eq!(sda.path, "/dev/sda", "falls back to the kernel path");
    }

    #[test]
    fn failed_lsblk_is_an_internal_error() {
        let runner = FakeRunner::with_closure(|program, _| {
            (program == "lsblk").then(|| CommandOutput::failure("lsblk: not available"))
        });
        let err = discover_devices(&runner, PathBuf::from("/").as_path()).expect_err("must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("lsblk"));
    }
}
