//! The tunnel idle sweep (MAIN-404 AC-3).
//!
//! A tunnel holds a port on somebody's machine open to their whole tenant, and
//! nothing about the two ways it ends on purpose — `nook tunnels stop`, the
//! session exiting — covers the ordinary case of a person opening one and
//! walking away. Without this, that tunnel is held until the control plane
//! restarts.
//!
//! Every replica sweeps its OWN copies of the route table, which is safe only
//! because a removal is broadcast: two replicas deciding at the same moment
//! close the one tunnel once. The idle clock a replica reads is refreshed by
//! its peers as they serve traffic (`BusMessage::TunnelUsed`), so a tunnel busy
//! on one replica is not swept by another — to within the announce interval,
//! which is a quarter of the window.

use std::time::Duration;

use crate::state::AppState;

pub fn start(state: AppState) {
    // Nothing to sweep where the surface is off: with no `TUNNEL_DOMAIN`,
    // nothing can create a tunnel in the first place.
    if state.cfg.tunnel_domain.is_none() {
        return;
    }
    let idle = state.cfg.tunnel_idle_secs;
    if idle == 0 {
        tracing::info!("tunnel idle sweep disabled (TUNNEL_IDLE_SECS=0) — tunnels are held until they are stopped");
        return;
    }
    tokio::spawn(async move {
        let window = Duration::from_secs(idle);
        // Four scans a window, so a tunnel is torn down within a quarter of the
        // window of going idle rather than up to a whole one late. Bounded at
        // both ends: a tiny window must not turn this into a spin, and a very
        // long one must not leave the sweep asleep for hours.
        let scan = Duration::from_secs((idle / 4).clamp(10, 300));
        tracing::info!(
            idle_secs = idle,
            scan_secs = scan.as_secs(),
            "tunnel idle sweep started"
        );
        run(state, window, scan).await;
    });
}

async fn run(state: AppState, window: Duration, scan: Duration) {
    loop {
        tokio::time::sleep(scan).await;
        for tunnel in state.registry.sweep_idle_tunnels(window) {
            crate::routes::tunnels::closed(&state, &tunnel, "idle").await;
        }
    }
}
