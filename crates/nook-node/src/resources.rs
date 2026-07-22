//! Live resource sampling for heartbeats. A `System` is kept between samples
//! because CPU utilization is measured as a delta across refreshes.

use nook_types::NodeResources;
use sysinfo::System;

pub struct Sampler {
    sys: System,
}

impl Sampler {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self { sys }
    }

    pub fn sample(&mut self) -> NodeResources {
        // CPU% needs two refreshes with a gap; the heartbeat interval provides
        // the gap between calls, so a single refresh here reads the delta.
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let cpu_percent = self.sys.global_cpu_usage();
        NodeResources {
            cpu_percent,
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            load_avg1: System::load_average().one,
            active_sessions: crate::tmux::list_nook_sessions().len() as u32,
        }
    }
}
