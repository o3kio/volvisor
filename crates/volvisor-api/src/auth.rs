//! Scoped credentials, role enforcement and the request audit event.
//!
//! ## Roles
//!
//! Three named roles, ordered by privilege:
//!
//! - **read-only** — read routes (`GET /v2/volumes`, capabilities,
//!   `/metrics`, migration observation);
//! - **operator** — every consumer mutation (volume lifecycle, grow, move,
//!   attach/detach, migration prepare/transfer/abort);
//! - **admin** — everything an operator may do plus the `/v2/admin/...`
//!   surface (device enrollment, lending, nearline adoption, cut-marker
//!   repair and mobility eligibility).
//!
//! The legacy top-level `admin_token` is a single admin credential. It
//! preserves the pre-scope behavior exactly: mutations/admin require it, and
//! read routes stay open unless a `[credentials]` table is configured. The
//! tokenless posture is still fail-closed for mutations/admin; a tokenless
//! daemon may only bind loopback.
//!
//! ## Read authorization
//!
//! When any scoped credential is configured, volume metadata, capabilities
//! and `/metrics` require at least read-only. `/healthz` stays open for
//! probes. When no scoped credential exists (loopback dev/test, or the legacy
//! admin-token-only mode) reads remain open; `docs/security/deployment.md`
//! states the threat model for that compatibility posture.
//!
//! ## Audit
//!
//! Every request (except a successful liveness probe) produces one structured
//! `kind = "audit"` event carrying the resolved principal and role, method,
//! matched route, status/outcome and — when the operation reached the journal
//! pipeline — its `operation_id`. Tokens are never included.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::json_response;
pub(crate) use crate::extract::{bearer_token, fixed_time_eq};
use crate::state::SharedState;

/// A scoped role. Ordering is privilege ordering: `ReadOnly < Operator <
/// Admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read-only access.
    ReadOnly,
    /// Consumer mutations, no `/v2/admin` surface.
    Operator,
    /// Full access.
    Admin,
}

impl Role {
    /// The stable name used in audit events and configuration summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::ReadOnly => "read-only",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

/// One named bearer credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    /// Stable principal id, used in audit events (never the token).
    pub id: String,
    /// The bearer token value. Never logged.
    pub token: String,
    /// The role this credential grants.
    pub role: Role,
}

/// The authenticated request principal, inserted into request extensions by
/// [`authorize_and_audit`] and read by the role extractors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    /// Credential id (or `admin_token` for the legacy token).
    pub principal: String,
    /// The resolved role.
    pub role: Role,
}

/// The credential set and the read-protection posture derived from it.
#[derive(Clone, Debug, Default)]
pub struct AuthPolicy {
    scoped: Vec<Credential>,
    legacy_admin_token: Option<String>,
}

impl AuthPolicy {
    /// Build a policy with the legacy admin token (or none for tokenless
    /// loopback dev/test).
    #[must_use]
    pub fn new(legacy_admin_token: Option<String>) -> Self {
        Self {
            scoped: Vec::new(),
            legacy_admin_token,
        }
    }

    /// Attach scoped named credentials.
    #[must_use]
    pub fn with_credentials(mut self, credentials: Vec<Credential>) -> Self {
        self.scoped = credentials;
        self
    }

    /// Whether read routes require authentication: true exactly when scoped
    /// credentials are configured. The legacy admin-token-only mode keeps
    /// the documented P0 open-read compatibility posture.
    #[must_use]
    pub fn protect_reads(&self) -> bool {
        !self.scoped.is_empty()
    }

    /// Whether any credential is configured at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.scoped.is_empty() || self.legacy_admin_token.is_some()
    }

    /// Resolve a presented token to a principal.
    ///
    /// All comparisons walk the full credential set without an early exit,
    /// so the response time does not reveal which entry (or role) matched.
    #[must_use]
    pub fn authenticate(&self, presented: Option<&str>) -> Option<AuthContext> {
        let presented = presented?;
        let mut matched = None;
        for credential in &self.scoped {
            if fixed_time_eq(presented, &credential.token) {
                matched = Some(AuthContext {
                    principal: credential.id.clone(),
                    role: credential.role,
                });
            }
        }
        if let Some(legacy) = &self.legacy_admin_token {
            if fixed_time_eq(presented, legacy) {
                matched = Some(AuthContext {
                    principal: "admin_token".to_owned(),
                    role: Role::Admin,
                });
            }
        }
        matched
    }
}

/// The operation id attached to responses by the journal pipeline, read by
/// the audit middleware. This is intentionally a response extension, never a
/// wire header: it does not change the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationAuditId(pub String);

/// What a matched route requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Public,
    Read,
    Operator,
    Admin,
    /// The internal peer surface has its own daemon credential, enforced by
    /// `peer::RequirePeer`; the audit middleware only resolves the principal.
    Peer,
}

/// Map a matched route pattern and method onto its required access.
fn required_access(route: &str, method: &axum::http::Method) -> Access {
    use axum::http::Method;
    if route == "/healthz" || route == "/v2/healthz" {
        return Access::Public;
    }
    if route.starts_with("/v2/internal/peer") {
        return Access::Peer;
    }
    if route.starts_with("/v2/admin") {
        return Access::Admin;
    }
    if route == "/v2/vms/{vm_id}/check-mobility" {
        // Eligibility observation is admin-gated today (it reveals host
        // placement and VMM state); keep that posture under scoped roles.
        return Access::Admin;
    }
    match route {
        "/metrics" | "/v2/capabilities" => Access::Read,
        "/v2/volumes" | "/v2/volumes/{volume_id}" | "/v2/migrations/{migration_id}" => {
            if *method == Method::GET {
                Access::Read
            } else {
                Access::Operator
            }
        }
        "/v2/volumes/{volume_id}/attach"
        | "/v2/volumes/{volume_id}/detach"
        | "/v2/volumes/{volume_id}/attachment-state"
        | "/v2/volumes/{volume_id}/grow"
        | "/v2/volumes/{volume_id}/move-backing"
        | "/v2/migrations"
        | "/v2/migrations/{migration_id}/transfer"
        | "/v2/migrations/{migration_id}/abort" => Access::Operator,
        // Unknown/fallback routes have no protected resource.
        _ => Access::Public,
    }
}

/// Authenticate, authorize and audit one request.
///
/// This is the single authentication point. The role extractors in
/// `extract` re-check the inserted [`AuthContext`] as defense in depth; they
/// never perform a second token comparison.
pub(crate) async fn authorize_and_audit(
    State(state): State<SharedState>,
    mut request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(
            || request.uri().path().to_owned(),
            |matched| matched.as_str().to_owned(),
        );
    let method = request.method().clone();
    let access = required_access(&route, &method);
    let presented = bearer_token(request.headers());
    let context = state.auth.authenticate(presented);

    let (principal, role) = match (&context, access) {
        (Some(context), _) => (context.principal.clone(), context.role.as_str().to_owned()),
        (None, Access::Peer) => resolve_peer_principal(&state, presented),
        (None, _) => ("anonymous".to_owned(), "none".to_owned()),
    };

    let denial = authorize(&state, access, context.as_ref());
    if let Some(response) = denial {
        audit(&principal, &role, &method, &route, &response, None);
        return response;
    }

    if let Some(context) = context {
        request.extensions_mut().insert(context);
    }
    let response = next.run(request).await;
    let operation_id = response
        .extensions()
        .get::<OperationAuditId>()
        .map(|id| id.0.clone());
    // Liveness probes are deliberately not audited on success: they are
    // high-frequency and carry no caller identity. A failed probe is still
    // an auditable event.
    if !(route == "/healthz" || route == "/v2/healthz") || !response.status().is_success() {
        audit(
            &principal,
            &role,
            &method,
            &route,
            &response,
            operation_id.as_deref(),
        );
    }
    response
}

/// Resolve the peer principal for the audit trail without performing the
/// peer authorization itself (`peer::RequirePeer` owns that boundary).
fn resolve_peer_principal(state: &SharedState, presented: Option<&str>) -> (String, String) {
    let authenticated = state
        .peer_token
        .as_deref()
        .is_some_and(|expected| presented.is_some_and(|token| fixed_time_eq(token, expected)));
    if authenticated {
        ("peer".to_owned(), "peer".to_owned())
    } else {
        ("anonymous".to_owned(), "none".to_owned())
    }
}

/// Apply the route's access rule. `None` means allowed.
fn authorize(
    state: &SharedState,
    access: Access,
    context: Option<&AuthContext>,
) -> Option<Response> {
    match access {
        Access::Public | Access::Peer => None,
        Access::Read => {
            if !state.auth.protect_reads() {
                return None;
            }
            match context {
                Some(context) if context.role >= Role::ReadOnly => None,
                Some(_) => Some(forbidden_response(
                    "the presented credential has no read permission",
                )),
                None => Some(unauthorized_response(
                    "read routes require a valid read-only, operator or admin bearer token",
                )),
            }
        }
        Access::Operator => match context {
            Some(context) if context.role >= Role::Operator => None,
            Some(_) => Some(forbidden_response(
                "the presented credential is read-only; operator or admin is required",
            )),
            None if !state.auth.is_configured() => Some(unauthorized_response(
                "admin_token is not configured; mutating endpoints are disabled (fail closed)",
            )),
            None => Some(unauthorized_response(
                "mutating operations require a valid operator or admin bearer token",
            )),
        },
        Access::Admin => match context {
            Some(context) if context.role >= Role::Admin => None,
            Some(_) => Some(forbidden_response(
                "the presented credential is not an admin credential",
            )),
            None if !state.auth.is_configured() => Some(unauthorized_response(
                "admin_token is not configured; mutating endpoints are disabled (fail closed)",
            )),
            None => Some(unauthorized_response(
                "the admin surface requires a valid admin bearer token",
            )),
        },
    }
}

/// Emit one structured audit event. Tokens never enter this function.
fn audit(
    principal: &str,
    role: &str,
    method: &axum::http::Method,
    route: &str,
    response: &Response,
    operation_id: Option<&str>,
) {
    let status = response.status();
    let outcome = if status.is_success() || status.is_redirection() {
        "success"
    } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        "denied"
    } else {
        "failure"
    };
    tracing::info!(
        kind = "audit",
        principal = %principal,
        role = %role,
        method = %method,
        route = %route,
        status = status.as_u16(),
        outcome = %outcome,
        operation_id = operation_id.unwrap_or(""),
        "request"
    );
}

fn unauthorized_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "UNAUTHORIZED",
        "message": message,
    });
    json_response(StatusCode::UNAUTHORIZED, &body)
}

fn forbidden_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "FORBIDDEN",
        "message": message,
    });
    json_response(StatusCode::FORBIDDEN, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn policy() -> AuthPolicy {
        AuthPolicy::new(None).with_credentials(vec![
            Credential {
                id: "reader".to_owned(),
                token: "read-token".to_owned(),
                role: Role::ReadOnly,
            },
            Credential {
                id: "operator".to_owned(),
                token: "op-token".to_owned(),
                role: Role::Operator,
            },
            Credential {
                id: "admin".to_owned(),
                token: "admin-token".to_owned(),
                role: Role::Admin,
            },
        ])
    }

    #[test]
    fn every_role_resolves_to_its_named_principal() {
        let policy = policy();
        assert_eq!(
            policy.authenticate(Some("read-token")),
            Some(AuthContext {
                principal: "reader".to_owned(),
                role: Role::ReadOnly
            })
        );
        assert_eq!(
            policy.authenticate(Some("op-token")),
            Some(AuthContext {
                principal: "operator".to_owned(),
                role: Role::Operator
            })
        );
        assert_eq!(
            policy.authenticate(Some("admin-token")),
            Some(AuthContext {
                principal: "admin".to_owned(),
                role: Role::Admin
            })
        );
        assert_eq!(policy.authenticate(Some("wrong")), None);
        assert_eq!(policy.authenticate(None), None);
    }

    #[test]
    fn legacy_admin_token_is_an_admin_principal() {
        let policy = AuthPolicy::new(Some("legacy".to_owned()));
        assert_eq!(
            policy.authenticate(Some("legacy")),
            Some(AuthContext {
                principal: "admin_token".to_owned(),
                role: Role::Admin
            })
        );
        assert!(
            !policy.protect_reads(),
            "legacy mode keeps the P0 read posture"
        );
    }

    #[test]
    fn scoped_credentials_protect_reads() {
        assert!(policy().protect_reads());
        assert!(!AuthPolicy::new(None).protect_reads());
        assert!(!AuthPolicy::new(Some("legacy".to_owned())).protect_reads());
    }

    #[test]
    fn route_matrix_maps_roles() {
        for (route, method, expected) in [
            ("/healthz", Method::GET, Access::Public),
            ("/v2/healthz", Method::GET, Access::Public),
            ("/metrics", Method::GET, Access::Read),
            ("/v2/capabilities", Method::GET, Access::Read),
            ("/v2/volumes", Method::GET, Access::Read),
            ("/v2/volumes", Method::POST, Access::Operator),
            ("/v2/volumes/{volume_id}", Method::GET, Access::Read),
            ("/v2/volumes/{volume_id}", Method::DELETE, Access::Operator),
            (
                "/v2/volumes/{volume_id}/attach",
                Method::POST,
                Access::Operator,
            ),
            ("/v2/admin/devices", Method::GET, Access::Admin),
            (
                "/v2/vms/{vm_id}/check-mobility",
                Method::POST,
                Access::Admin,
            ),
            ("/v2/migrations", Method::POST, Access::Operator),
            ("/v2/migrations/{migration_id}", Method::GET, Access::Read),
            (
                "/v2/migrations/{migration_id}/transfer",
                Method::POST,
                Access::Operator,
            ),
            ("/v2/internal/peer/health", Method::GET, Access::Peer),
            ("/unknown", Method::GET, Access::Public),
        ] {
            assert_eq!(
                required_access(route, &method),
                expected,
                "route {route} {method}"
            );
        }
    }

    #[test]
    fn role_ordering_is_privilege_ordering() {
        assert!(Role::Admin > Role::Operator);
        assert!(Role::Operator > Role::ReadOnly);
    }
}
