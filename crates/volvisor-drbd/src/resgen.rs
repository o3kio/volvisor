//! Deterministic DRBD resource-definition generation and parsing.
//!
//! Volvisor owns exactly one `.res` file per resource, named
//! `volvisor-<resource>.res` under the configured directory, and every
//! `drbdadm` mutation is scoped to it through `-c` (no global
//! `/etc/drbd.conf` reliance, no chance of touching a foreign resource —
//! AGENTS rule 7). The generated shape follows `drbd.conf(5)`
//! (drbd-utils 9.x): a resource-level `device /dev/drbdN minor N;`,
//! `disk /dev/<vg>/<lv>;`, `meta-disk internal;`, one `on <host>`
//! section per node (local `node-id 0`, peer `node-id 1`) each carrying
//! `address ipv4 <addr>:<port>;`, and a `net` section with the
//! `protocol` letter, `cram-hmac-alg` and the `shared-secret` (peer
//! authentication requires the hash algorithm to be set alongside the
//! secret per the man page).
//!
//! The file is written atomically at mode `0600` — it contains the
//! shared secret. The secret is read at generation time from a path
//! reference (never configured inline), is never stored in provider
//! state, and never appears in an error detail: [`read_shared_secret`]
//! failures name only the path and the I/O error.
//!
//! [`parse_resource_file`] is shared by the provider (ownership
//! verification: the file must still name our resource, minor,
//! protocol, disk and node set) and by the test world's fake `drbdadm`
//! (which executes the real file's contents), so the two can never
//! drift apart.

use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use volvisor_types::{ApiError, ApiErrorCode};

use crate::state::ReplicationMode;

/// The header comment marking a file as volvisor-managed.
pub const RESOURCE_HEADER: &str = "# managed by volvisor (drbd9-nearline-prototype); do not edit.";

/// The hash algorithm configured alongside `shared-secret` for peer
/// authentication (`cram-hmac-alg`; drbd.conf(5) states peer
/// authentication is only active with both set). `sha1` is the
/// algorithm drbd-utils documents for this HMAC use; it authenticates
/// the peer, it does not protect data confidentiality.
/// (ASSUMPTION(unverified): that the man-page-recommended `sha1` is
/// accepted by the peer's drbd-utils build; a mismatch fails at
/// `connect` time and surfaces as an unhealthy resource, never as
/// silent unauthenticated traffic.)
const CRAM_HMAC_ALG: &str = "sha1";

/// One volvisor-generated DRBD resource definition.
///
/// The local node always carries `node-id 0` and the peer `node-id 1`
/// (drbd.conf(5) allows 0..=16; two nodes is the P3 deployment shape).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceDefinition {
    /// Resource name (== backing LV name; injective per volume).
    pub resource_name: String,
    /// Allocated DRBD minor (`/dev/drbd<minor>`).
    pub minor: u32,
    /// Replication protocol fixed at create.
    pub protocol: ReplicationMode,
    /// Local `on` name (must match `uname -n`).
    pub local_node: String,
    /// Local IPv4 address (dotted quad, no port).
    pub local_address: String,
    /// Local replication port.
    pub local_port: u16,
    /// Peer `on` name (operator-provisioned host).
    pub peer_node: String,
    /// Peer IPv4 address (dotted quad, no port).
    pub peer_address: String,
    /// Peer replication port.
    pub peer_port: u16,
    /// Local backing device path (`/dev/<vg>/<lv>`). The P3 operator
    /// model deploys the identical definition on the peer, so the same
    /// path is assumed to exist there (ASSUMPTION(unverified): peer
    /// backing path equals the local one; see the plan §7).
    pub disk_path: String,
    /// The peer shared secret (never logged, never persisted in state).
    pub shared_secret: String,
}

impl ResourceDefinition {
    /// Render the resource definition text (the exact bytes written to
    /// the `.res` file).
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{RESOURCE_HEADER}\n\
             resource {name} {{\n\
             \x20   device /dev/drbd{minor} minor {minor};\n\
             \x20   disk {disk};\n\
             \x20   meta-disk internal;\n\
             \x20   on {local_node} {{\n\
             \x20       node-id 0;\n\
             \x20       address ipv4 {local_address}:{local_port};\n\
             \x20   }}\n\
             \x20   on {peer_node} {{\n\
             \x20       node-id 1;\n\
             \x20       address ipv4 {peer_address}:{peer_port};\n\
             \x20   }}\n\
             \x20   net {{\n\
             \x20       protocol {protocol};\n\
             \x20       cram-hmac-alg {cram};\n\
             \x20       shared-secret \"{secret}\";\n\
             \x20   }}\n\
             }}\n",
            name = self.resource_name,
            minor = self.minor,
            disk = self.disk_path,
            local_node = self.local_node,
            local_address = self.local_address,
            local_port = self.local_port,
            peer_node = self.peer_node,
            peer_address = self.peer_address,
            peer_port = self.peer_port,
            protocol = self.protocol.as_letter(),
            cram = CRAM_HMAC_ALG,
            secret = self.shared_secret,
        )
    }

    /// Write the definition atomically to
    /// `<config_dir>/volvisor-<resource>.res` at mode `0600` and return
    /// the path.
    ///
    /// # Errors
    /// `INTERNAL` on any I/O failure; the previous file (if any) is
    /// left intact (write-to-tmp, fsync, rename, fsync directory).
    pub fn write(&self, config_dir: &Path) -> Result<PathBuf, ApiError> {
        let path = res_file_path(config_dir, &self.resource_name);
        write_file_atomic_0600(&path, self.render().as_bytes())?;
        Ok(path)
    }
}

/// The resource-file path for a resource name.
#[must_use]
pub fn res_file_path(config_dir: &Path, resource: &str) -> PathBuf {
    config_dir.join(format!("volvisor-{resource}.res"))
}

/// Write `bytes` to `path` atomically at mode `0600`.
///
/// Shared by the resource generator; the same tmp-write + fsync +
/// rename + directory-fsync discipline the state module uses. Because
/// the file carries the shared secret, it is never world-readable.
pub(crate) fn write_file_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let tmp_path = {
        let mut os_name = path.as_os_str().to_owned();
        os_name.push(".tmp");
        PathBuf::from(os_name)
    };
    let result = (|| {
        let mut file = crate::state::create_owner_only_file(&tmp_path)
            .map_err(|e| internal(format!("failed to create {}: {e}", tmp_path.display())))?;
        file.write_all(bytes)
            .map_err(|e| internal(format!("failed to write {}: {e}", tmp_path.display())))?;
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {}: {e}", tmp_path.display())))?;
        drop(file);
        fs::rename(&tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {} to {}: {e}",
                tmp_path.display(),
                path.display()
            ))
        })?;
        let dir = fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|e| internal(format!("failed to open parent of {}: {e}", path.display())))?;
        dir.sync_all()
            .map_err(|e| internal(format!("failed to fsync parent of {}: {e}", path.display())))?;
        Ok(())
    })();
    if result.is_err() {
        drop(fs::remove_file(&tmp_path));
    }
    result
}

/// Read the peer shared secret from a path reference.
///
/// The value is trimmed and must be non-empty. It is never included in
/// any error detail — failures name only the path and the I/O error
/// (the secret is peer-authentication material; a leak through a log
/// line would defeat it).
///
/// # Errors
/// `INTERNAL` when the file cannot be read or is empty. Keeping the
/// file owner-only (`0600`) is operator responsibility, recorded in the
/// provider documentation. (ASSUMPTION(unverified): drbd-utils itself
/// imposes no permission requirement on this file; `0600` is volvisor's
/// own hygiene requirement per the plan's operator contract.)
pub fn read_shared_secret(path: &Path) -> Result<String, ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let content = fs::read_to_string(path).map_err(|e| {
        internal(format!(
            "failed to read shared secret file {}: {e}",
            path.display()
        ))
    })?;
    let secret = content.trim();
    if secret.is_empty() {
        return Err(internal(format!(
            "shared secret file {} is empty",
            path.display()
        )));
    }
    Ok(secret.to_owned())
}

/// One `on <host>` section of a parsed resource file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedNode {
    /// The `on` host name.
    pub name: String,
    /// The `node-id` value, when present.
    pub node_id: Option<u32>,
    /// The `address` value (`<addr>:<port>`; the address family word,
    /// if present, is consumed and dropped).
    pub address: Option<String>,
}

impl ParsedNode {
    /// The port of the `address`, when present and shaped `<ip>:<port>`.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.address
            .as_ref()
            .and_then(|address| address.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok())
    }
}

/// The structural subset of a parsed resource file.
///
/// Deliberately does **not** expose the shared secret value — only
/// whether one is present ([`ParsedResource::has_shared_secret`]) — so
/// verification code can check completeness without ever handling the
/// secret outside the generation path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedResource {
    /// The resource name.
    pub name: String,
    /// The minor from the `device` line (explicit `minor N` or parsed
    /// from the `/dev/drbdN` path).
    pub minor: Option<u32>,
    /// The `net` protocol letter (`A`/`B`/`C`).
    pub protocol: Option<String>,
    /// All `disk` paths in the resource.
    pub disks: Vec<String>,
    /// All `on` sections, in file order.
    pub nodes: Vec<ParsedNode>,
    /// Whether a non-empty `shared-secret` is configured.
    pub has_shared_secret: bool,
}

/// Parse a resource-definition file.
///
/// Line-oriented and comment-aware (`#` to end of line); braces and
/// semicolons are self-delimiting tokens, quoted values are taken as
/// single tokens. Only the structural subset of [`ParsedResource`] is
/// extracted; unknown statements are skipped (forward tolerance), but a
/// file without a resource name is rejected.
///
/// # Errors
/// `INTERNAL` when the content has no `resource <name>` statement or
/// ends with unbalanced braces.
// A single forward pass whose arms mirror the verified drbd.conf(5)
// statement set; extracting arms would separate the depth tracking
// from the statements it guards.
#[allow(clippy::too_many_lines)]
pub fn parse_resource_file(content: &str) -> Result<ParsedResource, ApiError> {
    let tokens = tokenize(content);
    let mut parsed = ParsedResource::default();
    let mut depth = 0usize;
    let mut in_net = false;
    let mut current_node: Option<ParsedNode> = None;

    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        let next = || tokens.get(index + 1).map(String::as_str);
        match token {
            "{" => depth += 1,
            "}" => {
                if depth == 2 {
                    if let Some(node) = current_node.take() {
                        parsed.nodes.push(node);
                    }
                    in_net = false;
                }
                depth = depth.saturating_sub(1);
            }
            "resource" if depth == 0 => {
                if let Some(name) = next() {
                    if parsed.name.is_empty() {
                        name.clone_into(&mut parsed.name);
                    }
                }
            }
            "device" if depth == 1 => {
                if let Some(path) = next() {
                    // `device /dev/drbdN minor N;` — the minor may be
                    // explicit or derivable from the device path.
                    if parsed.minor.is_none() {
                        parsed.minor = path
                            .rsplit_once("drbd")
                            .and_then(|(_, number)| number.parse().ok());
                    }
                    if tokens.get(index + 2).map(String::as_str) == Some("minor") {
                        if let Some(minor) =
                            tokens.get(index + 3).and_then(|value| value.parse().ok())
                        {
                            parsed.minor = Some(minor);
                        }
                    }
                }
            }
            "disk" if depth == 1 => {
                if let Some(disk) = next() {
                    parsed.disks.push(disk.to_owned());
                }
            }
            "meta-disk" if depth == 1 => {}
            "on" if depth == 1 => {
                if let Some(name) = next() {
                    current_node = Some(ParsedNode {
                        name: name.to_owned(),
                        node_id: None,
                        address: None,
                    });
                }
            }
            "net" if depth == 1 => in_net = true,
            "node-id" if depth == 2 => {
                if let Some(node) = current_node.as_mut() {
                    node.node_id = next().and_then(|value| value.parse().ok());
                }
            }
            "address" if depth == 2 && !in_net => {
                // `address [family] <addr>:<port>;`
                let mut address = next();
                if matches!(address, Some("ipv4" | "ipv6")) {
                    address = tokens.get(index + 2).map(String::as_str);
                }
                if let Some(address) = address {
                    if let Some(node) = current_node.as_mut() {
                        node.address = Some(address.to_owned());
                    }
                }
            }
            "protocol" if depth == 2 && in_net => {
                parsed.protocol = next().map(str::to_owned);
            }
            "shared-secret" if depth == 2 && in_net => {
                if let Some(value) = next() {
                    let trimmed = value.trim_matches('"');
                    parsed.has_shared_secret = !trimmed.is_empty();
                }
            }
            _ => {}
        }
        index += 1;
    }
    if parsed.name.is_empty() {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            "failed to parse resource file: no `resource <name>` statement",
        ));
    }
    if depth != 0 {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            "failed to parse resource file: unbalanced braces",
        ));
    }
    Ok(parsed)
}

/// Split resource-file text into tokens: comments (`#` to end of line)
/// removed, braces and semicolons self-delimiting, whitespace separated.
fn tokenize(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for line in content.lines() {
        let line = line.split_once('#').map_or(line, |(before, _)| before);
        let mut current = String::new();
        for character in line.chars() {
            match character {
                '{' | '}' | ';' => {
                    if !current.is_empty() {
                        tokens.push(current.clone());
                        current.clear();
                    }
                    tokens.push(character.to_string());
                }
                character if character.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(current.clone());
                        current.clear();
                    }
                }
                character => current.push(character),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
    }
    tokens
}

/// Validate an IPv4 dotted-quad address (used by the provider config).
#[must_use]
pub fn is_ipv4_literal(address: &str) -> bool {
    address.parse::<Ipv4Addr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> ResourceDefinition {
        ResourceDefinition {
            resource_name: "vol-vol-1-abcd0123".to_owned(),
            minor: 7,
            protocol: ReplicationMode::A,
            local_node: "node-a".to_owned(),
            local_address: "10.0.0.1".to_owned(),
            local_port: 7901,
            peer_node: "node-b".to_owned(),
            peer_address: "10.0.0.2".to_owned(),
            peer_port: 7901,
            disk_path: "/dev/vgdrbd/vol-vol-1-abcd0123".to_owned(),
            shared_secret: "sekrit".to_owned(),
        }
    }

    #[test]
    fn render_parse_round_trip_is_lossless() {
        let rendered = definition().render();
        let parsed = parse_resource_file(&rendered).expect("parse");
        assert_eq!(parsed.name, "vol-vol-1-abcd0123");
        assert_eq!(parsed.minor, Some(7));
        assert_eq!(parsed.protocol.as_deref(), Some("A"));
        assert_eq!(
            parsed.disks,
            vec!["/dev/vgdrbd/vol-vol-1-abcd0123".to_owned()]
        );
        assert_eq!(parsed.nodes.len(), 2);
        assert_eq!(parsed.nodes[0].name, "node-a");
        assert_eq!(parsed.nodes[0].node_id, Some(0));
        assert_eq!(parsed.nodes[0].address.as_deref(), Some("10.0.0.1:7901"));
        assert_eq!(parsed.nodes[0].port(), Some(7901));
        assert_eq!(parsed.nodes[1].name, "node-b");
        assert_eq!(parsed.nodes[1].node_id, Some(1));
        assert_eq!(parsed.nodes[1].port(), Some(7901));
        assert!(parsed.has_shared_secret);
    }

    #[test]
    fn rendered_file_carries_the_managed_header() {
        let rendered = definition().render();
        assert!(rendered.starts_with("# managed by volvisor"));
        assert!(rendered.contains("meta-disk internal;"));
        assert!(rendered.contains("cram-hmac-alg sha1;"));
        assert!(rendered.contains("protocol A;"));
    }

    #[test]
    fn parse_skips_comments_and_unknown_statements() {
        let content = "# a comment line\n\
                       resource r0 { # trailing comment\n\
                       weird-stuff whatever;\n\
                       disk /dev/vg/lv;\n\
                       }\n";
        let parsed = parse_resource_file(content).expect("parse");
        assert_eq!(parsed.name, "r0");
        assert_eq!(parsed.disks, vec!["/dev/vg/lv".to_owned()]);
    }

    #[test]
    fn parse_rejects_nameless_and_unbalanced_files() {
        assert!(parse_resource_file("disk /dev/vg/lv;").is_err());
        assert!(parse_resource_file("resource r0 { disk /dev/vg/lv;").is_err());
    }

    #[test]
    fn write_creates_owner_only_file_and_residue_free_rewrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = res_file_path(dir.path(), "vol-x");
        write_file_atomic_0600(&path, b"first").expect("write");
        assert!(path.exists());
        write_file_atomic_0600(&path, b"second").expect("rewrite");
        assert_eq!(fs::read_to_string(&path).expect("read"), "second");
        assert!(!dir.path().join("vol-x.res.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "resource files are owner-only");
        }
    }

    #[test]
    fn read_shared_secret_trims_and_never_leaks_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        fs::write(&path, b"  sekrit \n").expect("write");
        assert_eq!(read_shared_secret(&path).expect("secret"), "sekrit");

        // Empty file: typed error naming the path only.
        fs::write(&path, b"   \n").expect("write");
        let err = read_shared_secret(&path).expect_err("empty secret");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.detail.contains("sekrit"), "never leak the value");

        // Missing file: typed error naming the path and I/O error only.
        let err = read_shared_secret(&dir.path().join("missing")).expect_err("missing");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.detail.contains("sekrit"));
    }

    #[test]
    fn ipv4_literal_validation() {
        assert!(is_ipv4_literal("10.0.0.1"));
        assert!(!is_ipv4_literal("10.0.0"));
        assert!(!is_ipv4_literal("node-a"));
        assert!(!is_ipv4_literal("10.0.0.1:7901"));
    }
}
