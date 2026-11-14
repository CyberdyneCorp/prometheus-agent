//! The seam between the daemon and its web dashboard listener (spec:
//! web-dashboard, Listener lifecycle within the daemon). The dashboard
//! (`pmt-web`) depends on this crate, so the daemon reaches it only through
//! this trait: the control channel's `dashboard` requests go to the
//! controller the daemon installed with [`crate::Server::set_dashboard`].

use pmt_rpc::control::{DashboardAction, DashboardState};

use crate::host::HostFuture;

/// Starts, reports and stops the dashboard listener.
pub trait DashboardControl: Send + Sync {
    /// Answers one `dashboard` control request.
    fn control(&self, action: DashboardAction) -> HostFuture<'_, DashboardState>;
}
