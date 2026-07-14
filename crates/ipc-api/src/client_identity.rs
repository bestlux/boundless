//! Server-verified identity of a local control-plane client connection.
//!
//! On Windows the named-pipe server resolves this at accept time from the
//! actual pipe client (process token SID and client session id) and attaches
//! it as tonic `ConnectInfo`. Handlers that gate on caller identity — the
//! user-session input broker APIs — must use these fields and never a
//! client-supplied claim. Absent or partially resolved identity means the
//! caller could not be verified; identity-gated handlers fail closed on it.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlClientIdentity {
    /// Process id resolved from the connected pipe handle, when available.
    pub process_id: Option<u32>,
    /// String SID of the account owning the client process, when resolvable.
    pub user_sid: Option<String>,
    /// Windows session id of the pipe client, when resolvable.
    pub session_id: Option<u32>,
}
