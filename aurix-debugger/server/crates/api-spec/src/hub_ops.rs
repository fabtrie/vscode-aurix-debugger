//! The hub's own operations: sessions and profiles. The hub serves them next to the session
//! operations of its workers.

use crate::types::*;

api! {
    trait HubApi; table HUB_OPS; dispatch hub_dispatch; scope Hub;

    /// What is serving: the hub, its version, address, sessions and whether an update waits.
    ///
    /// Always answers at once.
    op hub_info [hub]: GET "/api/v1/hub" (NoArgs) -> HubInfo;

    /// List the open sessions: one per board, each with its targets, ports, launch state and
    /// control lease holder.
    op list_sessions [hub]: GET "/api/v1/sessions" (NoArgs) -> SessionList;

    /// Open a session: start a worker for a board, connect, and run the launch steps.
    ///
    /// Give a `profile` (see `list_profiles`), or `connect` with the configuration scripts, or
    /// both: the arguments override the profile. Without `launch` (and without a `[launch]` part
    /// in the profile) the session only connects. Opening a session connects to the hardware and
    /// may reset the target.
    ///
    /// Returns once the steps are done or after `wait_s` seconds with phase `starting`;
    /// `wait_session` waits for the rest. A failed step leaves the session open to inspect,
    /// unless `close_on_error`.
    op open_session [hub]: POST "/api/v1/sessions" (OpenSessionRequest) -> SessionInfo;

    /// Close a session: the worker disconnects from the board and ends. Connected VS Code
    /// sessions see the disconnect.
    op close_session [hub]: DELETE "/api/v1/sessions/{session}" (SessionRef) -> SessionList;

    /// The end of a session's log (the worker's console output), e.g. to see why `init` failed.
    op session_log [hub]: GET "/api/v1/sessions/{session}/log" (SessionLogRequest) -> SessionLog;

    /// List the session profiles stored on the hub machine.
    op list_profiles [hub]: GET "/api/v1/profiles" (NoArgs) -> ProfileList;
}
