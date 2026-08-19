//! Live resource sampling for heartbeats. A `System` is kept between samples
//! because CPU utilization is measured as a delta across refreshes.
//!
//! Free disk (MAIN-618) is here rather than in `capabilities` because it is a
//! LIVE number: a capability is detected once at connect, and a filesystem that
//! was roomy an hour ago is what this ticket exists for. The comparison against
//! the floor happens here too — the floor is stated on the machine and nowhere
//! else (NG-1), so the node is the only party that can phrase the shortage.

use nook_types::{DiskSample, NodeResources};
use std::path::{Path, PathBuf};
use sysinfo::{Disks, System};

use crate::loop_job::human_bytes;

/// Free space a node keeps in hand before it will claim loop work, when
/// `NOOK_MIN_FREE_DISK_GB` is unset. A build clones a repo, fills a target
/// directory and pulls images into a nested daemon; twenty gigabytes is the
/// room one needs without being so generous that a modest machine is cordoned.
pub const DEFAULT_MIN_FREE_DISK_GB: u64 = 20;

const GB: u64 = 1024 * 1024 * 1024;

pub struct Sampler {
    sys: System,
    min_free_bytes: u64,
    /// The paths whose filesystems loop work consumes, with the name a human
    /// reads. Resolved once: a machine's Docker root does not move, and
    /// `docker info` is a subprocess and a daemon round trip — too much to
    /// spend on every heartbeat.
    watched: Vec<(String, PathBuf)>,
}

impl Sampler {
    /// Takes the floor rather than reading it, because a misconfigured floor
    /// must stop the agent ONCE at startup ([`min_free_disk_bytes`]) rather
    /// than on each reconnect, after the node has already registered.
    pub fn new(min_free_bytes: u64) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            sys,
            min_free_bytes,
            watched: watched_paths(docker_root()),
        }
    }

    pub fn sample(&mut self) -> NodeResources {
        // CPU% needs two refreshes with a gap; the heartbeat interval provides
        // the gap between calls, so a single refresh here reads the delta.
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let cpu_percent = self.sys.global_cpu_usage();
        // Listed afresh rather than refreshed in place: free space is the whole
        // point of the sample, and a mount that appeared or went away since the
        // last heartbeat would otherwise be invisible until a reconnect.
        let disks = samples(&self.watched, &filesystems());
        NodeResources {
            cpu_percent,
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            load_avg1: System::load_average().one,
            active_sessions: crate::tmux::list_nook_sessions().len() as u32,
            disk_shortage: shortage(&disks, self.min_free_bytes),
            disks,
        }
    }
}

/// One mounted filesystem, as the grouping below needs it — so that logic is
/// testable without a machine that happens to have the right mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filesystem {
    pub mount_point: PathBuf,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

fn filesystems() -> Vec<Filesystem> {
    Disks::new_with_refreshed_list()
        .iter()
        .map(|d| Filesystem {
            mount_point: d.mount_point().to_path_buf(),
            free_bytes: d.available_space(),
            total_bytes: d.total_space(),
        })
        .collect()
}

/// This machine's floor, from its environment.
///
/// Read ONCE at agent startup, beside the plaintext check and for that check's
/// reason: a misconfiguration has to be a single loud stop. Resolved inside the
/// reconnect loop it would instead fail AFTER the node had registered — online
/// in the fleet, never heartbeating, retrying forever.
pub fn min_free_disk_bytes() -> Result<u64, String> {
    parse_min_free_disk_bytes(&std::env::var("NOOK_MIN_FREE_DISK_GB").unwrap_or_default())
}

/// The floor in bytes, from a raw `NOOK_MIN_FREE_DISK_GB`.
///
/// Unreadable is an ERROR where the neighbouring `parse_max_loop_jobs` takes a
/// default, and the asymmetry is deliberate: capacity is echoed back by `nook
/// get nodes` in a column a human is already reading, so a typo shows itself.
/// A floor that fell back to 20 would be invisible on a machine deliberately
/// set to 200 — and the symptom is the node claiming work it cannot finish,
/// which is exactly what this ticket exists to stop.
pub fn parse_min_free_disk_bytes(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(DEFAULT_MIN_FREE_DISK_GB * GB);
    }
    raw.parse::<u64>()
        .ok()
        .and_then(|gb| gb.checked_mul(GB))
        .ok_or_else(|| {
            format!(
                "NOOK_MIN_FREE_DISK_GB is {raw:?}, which is not a whole number of gigabytes \
                 (unset it for the {DEFAULT_MIN_FREE_DISK_GB} GB default)"
            )
        })
}

/// Docker's data root, as the daemon itself reports it. `None` when there is no
/// daemon to ask — a containerised node runs no builds, so it has no images or
/// volumes to run out of room for.
fn docker_root() -> Option<PathBuf> {
    let out = std::process::Command::new("docker")
        .args(["info", "--format", "{{.DockerRootDir}}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// What a loop job fills up, in the order a reader wants it.
///
/// The clone-cache ROOT, not the per-control-plane directory under it: the
/// question is which filesystem has room, and the root answers it even before
/// the first clone has created anything below.
fn watched_paths(docker_root: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut watched = vec![("job cache".to_string(), crate::loop_job::cache_root())];
    if let Some(root) = docker_root {
        watched.push(("Docker data root".to_string(), root));
    }
    watched
}

/// Which filesystem holds `path`: the longest mount point that is a prefix of
/// it. Longest wins because mount points nest — `/var/lib/docker` on its own
/// volume under a `/` that also exists is the case that matters here.
fn holding<'a>(filesystems: &'a [Filesystem], path: &Path) -> Option<&'a Filesystem> {
    filesystems
        .iter()
        .filter(|f| path.starts_with(&f.mount_point))
        .max_by_key(|f| f.mount_point.as_os_str().len())
}

/// The watched paths as reportable samples, one per FILESYSTEM.
///
/// Two paths on one filesystem collapse to a single sample naming both (AC-1):
/// left as two rows they would read as two independent pools, and an operator
/// looking at 15 GB twice would think there were 30.
pub fn samples(watched: &[(String, PathBuf)], filesystems: &[Filesystem]) -> Vec<DiskSample> {
    let mut out: Vec<DiskSample> = Vec::new();
    for (label, path) in watched {
        let Some(fs) = holding(filesystems, path) else {
            continue;
        };
        let mount_point = fs.mount_point.display().to_string();
        match out.iter_mut().find(|s| s.mount_point == mount_point) {
            Some(existing) => existing.label = format!("{}, {label}", existing.label),
            None => out.push(DiskSample {
                label: label.clone(),
                mount_point,
                free_bytes: fs.free_bytes,
                total_bytes: fs.total_bytes,
            }),
        }
    }
    out
}

/// The shortage sentence, or `None` when every sampled filesystem clears the
/// floor. Exactly AT the floor is fine — the floor is what a node keeps in
/// hand, not a number it must beat.
pub fn shortage(disks: &[DiskSample], min_free_bytes: u64) -> Option<String> {
    let short: Vec<String> = disks
        .iter()
        .filter(|d| d.free_bytes < min_free_bytes)
        .map(|d| {
            format!(
                "{} ({}) has {} free of {}",
                d.label,
                d.mount_point,
                human_bytes(d.free_bytes),
                human_bytes(d.total_bytes)
            )
        })
        .collect();
    if short.is_empty() {
        return None;
    }
    Some(format!(
        "below the {} free-disk floor: {}",
        human_bytes(min_free_bytes),
        short.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(mount: &str, free: u64, total: u64) -> Filesystem {
        Filesystem {
            mount_point: PathBuf::from(mount),
            free_bytes: free,
            total_bytes: total,
        }
    }

    fn watched(paths: &[(&str, &str)]) -> Vec<(String, PathBuf)> {
        paths
            .iter()
            .map(|(l, p)| (l.to_string(), PathBuf::from(p)))
            .collect()
    }

    #[test]
    fn an_unset_floor_is_twenty_gigabytes() {
        assert_eq!(
            parse_min_free_disk_bytes(""),
            Ok(DEFAULT_MIN_FREE_DISK_GB * GB)
        );
        assert_eq!(
            parse_min_free_disk_bytes("   "),
            Ok(DEFAULT_MIN_FREE_DISK_GB * GB)
        );
    }

    #[test]
    fn a_floor_is_read_in_gigabytes() {
        assert_eq!(parse_min_free_disk_bytes("50"), Ok(50 * GB));
        assert_eq!(parse_min_free_disk_bytes(" 5 "), Ok(5 * GB));
        // Zero is a legitimate "never hold work back", not a mistake.
        assert_eq!(parse_min_free_disk_bytes("0"), Ok(0));
    }

    #[test]
    fn a_malformed_floor_is_refused_rather_than_defaulted() {
        for raw in ["twenty", "20GB", "20.5", "-1", "99999999999999999999"] {
            let err = parse_min_free_disk_bytes(raw).unwrap_err();
            assert!(err.contains("NOOK_MIN_FREE_DISK_GB"), "{raw}: {err}");
        }
    }

    #[test]
    fn a_path_is_matched_to_its_longest_mount_point() {
        let mounts = vec![
            fs("/", 100, 200),
            fs("/var/lib/docker", 10, 50),
            fs("/home", 30, 60),
        ];
        let out = samples(
            &watched(&[
                ("job cache", "/home/ryan/.nook/clone-cache"),
                ("Docker data root", "/var/lib/docker"),
            ]),
            &mounts,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].mount_point, "/home");
        assert_eq!(out[0].free_bytes, 30);
        assert_eq!(out[1].mount_point, "/var/lib/docker");
        assert_eq!(out[1].free_bytes, 10);
    }

    #[test]
    fn one_filesystem_holding_both_paths_is_reported_once() {
        let out = samples(
            &watched(&[
                ("job cache", "/home/ryan/.nook/clone-cache"),
                ("Docker data root", "/var/lib/docker"),
            ]),
            &[fs("/", 42, 100)],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "job cache, Docker data root");
        assert_eq!(out[0].free_bytes, 42);
    }

    #[test]
    fn a_path_on_no_known_filesystem_is_simply_absent() {
        // Not an invented zero-byte sample: that would read as a full disk and
        // cordon the node over a mount table this code failed to understand.
        let out = samples(&watched(&[("job cache", "relative/path")]), &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn the_floor_holds_above_at_and_below() {
        let floor = 20 * GB;
        let at = |free| {
            vec![DiskSample {
                label: "job cache".into(),
                mount_point: "/".into(),
                free_bytes: free,
                total_bytes: 100 * GB,
            }]
        };
        assert_eq!(shortage(&at(21 * GB), floor), None);
        assert_eq!(shortage(&at(floor), floor), None);
        let short = shortage(&at(19 * GB), floor).expect("19 GB is below a 20 GB floor");
        assert!(short.contains("job cache"), "{short}");
        assert!(short.contains('/'), "{short}");
        assert!(short.contains("19.0 GiB"), "{short}");
    }

    #[test]
    fn either_filesystem_alone_is_enough_to_be_short() {
        let floor = 20 * GB;
        let pair = |cache_free, docker_free| {
            vec![
                DiskSample {
                    label: "job cache".into(),
                    mount_point: "/home".into(),
                    free_bytes: cache_free,
                    total_bytes: 100 * GB,
                },
                DiskSample {
                    label: "Docker data root".into(),
                    mount_point: "/var/lib/docker".into(),
                    free_bytes: docker_free,
                    total_bytes: 100 * GB,
                },
            ]
        };
        assert_eq!(shortage(&pair(50 * GB, 50 * GB), floor), None);
        let cache_short = shortage(&pair(GB, 50 * GB), floor).expect("cache is short");
        assert!(cache_short.contains("/home"), "{cache_short}");
        assert!(
            !cache_short.contains("/var/lib/docker"),
            "the roomy filesystem is not named: {cache_short}"
        );
        let docker_short = shortage(&pair(50 * GB, GB), floor).expect("docker is short");
        assert!(docker_short.contains("/var/lib/docker"), "{docker_short}");
        let both = shortage(&pair(GB, 2 * GB), floor).expect("both are short");
        assert!(
            both.contains("/home") && both.contains("/var/lib/docker"),
            "{both}"
        );
    }

    #[test]
    fn a_shared_filesystem_is_judged_once() {
        let out = samples(
            &watched(&[
                ("job cache", "/home/ryan/.nook/clone-cache"),
                ("Docker data root", "/var/lib/docker"),
            ]),
            &[fs("/", 5 * GB, 100 * GB)],
        );
        let short = shortage(&out, 20 * GB).expect("5 GB is below a 20 GB floor");
        assert_eq!(short.matches("has").count(), 1, "{short}");
        assert!(short.contains("job cache, Docker data root"), "{short}");
    }

    #[test]
    fn no_sample_at_all_is_no_shortage() {
        assert_eq!(shortage(&[], 20 * GB), None);
    }
}
