//! What the operating system says about this daemon and its machine, for the
//! activity view. Every figure is optional: a source the OS does not offer
//! (no `/proc`, no PSI, no cgroup) is `None`, never zero.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// Share of time some task waited on a resource (Linux PSI, "some" line), in percent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    pub avg10: f64,
    pub avg60: f64,
}

/// The control group the daemon runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cgroup {
    pub memory_current_bytes: u64,
    /// `None` when the group has no memory limit.
    pub memory_limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    pub rss_bytes: Option<u64>,
    /// User plus system CPU time the daemon has used, in seconds.
    pub cpu_secs: Option<f64>,
    pub threads: Option<u64>,
    /// Memory the machine can give out without swapping.
    pub mem_available_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub memory_pressure: Option<Pressure>,
    pub cpu_pressure: Option<Pressure>,
    /// Disk pressure of the whole machine, so it also shows another process loading the disk.
    pub io_pressure: Option<Pressure>,
    /// Bytes the daemon has caused to be written to storage; compare two readings for a rate.
    pub written_bytes: Option<u64>,
    /// Free space on the partition that holds the daemon's data; `None` reads the real disk.
    /// Only a source that is told its figures (see [`ManualResources`]) uses this.
    pub disk_free_bytes: Option<u64>,
    pub cgroup: Option<Cgroup>,
}

pub trait ResourceSource: Send + Sync {
    fn sample(&self) -> Resources;

    /// Bytes available to this daemon on the partition holding `dir`, if the OS says.
    fn disk_free(&self, _dir: &std::path::Path) -> Option<u64> {
        None
    }
}

/// Free bytes for unprivileged users on the partition holding `dir`.
pub fn free_space(dir: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a valid C string and `st` is a valid out pointer for statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled `st`.
    let st = unsafe { st.assume_init() };
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Reads `/proc` and the cgroup files; every figure the platform lacks stays `None`.
pub struct SystemResources;

impl ResourceSource for SystemResources {
    fn sample(&self) -> Resources {
        #[cfg(target_os = "linux")]
        {
            linux::sample()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Resources::default()
        }
    }

    fn disk_free(&self, dir: &std::path::Path) -> Option<u64> {
        free_space(dir)
    }
}

/// Figures that only change when told to; none until set.
#[derive(Default)]
pub struct ManualResources(Mutex<Resources>);

impl ManualResources {
    pub fn set(&self, resources: Resources) {
        *self.0.lock().unwrap() = resources;
    }
}

impl ResourceSource for ManualResources {
    fn sample(&self) -> Resources {
        self.0.lock().unwrap().clone()
    }

    fn disk_free(&self, dir: &std::path::Path) -> Option<u64> {
        self.0
            .lock()
            .unwrap()
            .disk_free_bytes
            .or_else(|| free_space(dir))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Cgroup, Pressure, Resources};

    pub fn sample() -> Resources {
        let status = std::fs::read_to_string("/proc/self/status").ok();
        let field = |text: &Option<String>, key: &str| -> Option<u64> {
            text.as_deref()?
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
        let kib = |v: Option<u64>| v.map(|k| k * 1024);
        Resources {
            rss_bytes: kib(field(&status, "VmRSS")),
            cpu_secs: cpu_secs(),
            threads: field(&status, "Threads"),
            mem_available_bytes: kib(field(&meminfo, "MemAvailable")),
            swap_used_bytes: field(&meminfo, "SwapTotal")
                .zip(field(&meminfo, "SwapFree"))
                .map(|(total, free)| total.saturating_sub(free) * 1024),
            memory_pressure: pressure("/proc/pressure/memory"),
            cpu_pressure: pressure("/proc/pressure/cpu"),
            io_pressure: pressure("/proc/pressure/io"),
            written_bytes: written_bytes(),
            disk_free_bytes: None,
            cgroup: cgroup(),
        }
    }

    fn cpu_secs() -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // The command name is parenthesised and may hold spaces: count from after it.
        let rest = stat.rsplit_once(')')?.1;
        let mut fields = rest.split_whitespace();
        // utime and stime are fields 14 and 15 of the line; `rest` starts at field 3.
        let utime: u64 = fields.nth(11)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (hz > 0).then(|| (utime + stime) as f64 / hz as f64)
    }

    fn written_bytes() -> Option<u64> {
        std::fs::read_to_string("/proc/self/io")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("write_bytes:"))?
            .trim()
            .parse()
            .ok()
    }

    fn pressure(path: &str) -> Option<Pressure> {
        let text = std::fs::read_to_string(path).ok()?;
        let some = text.lines().find(|l| l.starts_with("some "))?;
        let avg = |key: &str| -> Option<f64> {
            some.split_whitespace()
                .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))?
                .parse()
                .ok()
        };
        Some(Pressure {
            avg10: avg("avg10")?,
            avg60: avg("avg60")?,
        })
    }

    /// The memory figures of the cgroup (v2) this process is in.
    fn cgroup() -> Option<Cgroup> {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let path = own.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        let current = read("memory.current")?.trim().parse().ok()?;
        let limit = read("memory.max")?;
        Some(Cgroup {
            memory_current_bytes: current,
            memory_limit_bytes: limit.trim().parse().ok(),
        })
    }
}
