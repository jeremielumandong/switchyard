//! The Fleet dashboard's probe: one small, read-only POSIX shell script run over a Host's
//! SSH session, and pure parsers for what it prints (tested on recorded output from
//! Ubuntu, Alpine / BusyBox and RHEL-like systems).
//!
//! The script prints sections headed `@@name`; a missing tool leaves its section empty,
//! and every parser accepts that.

use serde::{Deserialize, Serialize};

/// The probe script. Read-only: it only runs `cat`, `uptime`, `df`, `free`, `nproc`,
/// `systemctl --failed` and `uname`.
pub const PROBE_SCRIPT: &str = "export LC_ALL=C; \
echo @@uptime; uptime 2>/dev/null; \
echo @@proc_uptime; cat /proc/uptime 2>/dev/null; \
echo @@loadavg; cat /proc/loadavg 2>/dev/null; \
echo @@nproc; (nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null); \
echo @@df; df -P 2>/dev/null; \
echo @@free; free -b 2>/dev/null; \
echo @@meminfo; cat /proc/meminfo 2>/dev/null; \
echo @@failed; if command -v systemctl >/dev/null 2>&1; then systemctl --failed --no-legend --plain 2>/dev/null; else echo @@nosystemd; fi; \
echo @@os; cat /etc/os-release 2>/dev/null; \
echo @@uname; uname -sr 2>/dev/null; \
echo @@end";

/// Disk or memory use at or above this percentage is flagged.
pub const FULL_PERCENT: f64 = 90.0;

/// One mounted file system (`df -P`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    /// Device or source.
    pub filesystem: String,
    /// Mount point.
    pub mount: String,
    /// Size in KiB (1024-byte blocks).
    pub size_kb: u64,
    /// Used KiB.
    pub used_kb: u64,
    /// Used percentage, as `df` computes it.
    pub use_percent: f64,
}

/// Memory, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// Total.
    pub total: u64,
    /// Available to new programs (`MemAvailable`, or free + buffers + cache).
    pub available: u64,
}

impl Memory {
    /// Used percentage (total − available).
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.total.saturating_sub(self.available)) as f64 * 100.0 / self.total as f64
    }
}

/// What the probe found on one Host.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// Seconds since boot.
    pub uptime_secs: Option<u64>,
    /// 1, 5 and 15 minute load averages.
    pub load: Option<[f64; 3]>,
    /// Online CPUs.
    pub cpus: Option<u32>,
    /// Real file systems (pseudo file systems and snaps left out).
    pub disks: Vec<Disk>,
    /// Memory.
    pub memory: Option<Memory>,
    /// Failed systemd units; `None` without systemd.
    pub failed_units: Option<Vec<String>>,
    /// Operating system (`PRETTY_NAME`).
    pub os: Option<String>,
    /// Kernel (`uname -sr`).
    pub kernel: Option<String>,
}

/// Something on a Host that needs attention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProbeFlag {
    /// A file system at or above [`FULL_PERCENT`].
    DiskFull {
        /// Mount point.
        mount: String,
        /// Used percentage.
        percent: f64,
    },
    /// Memory use at or above [`FULL_PERCENT`].
    MemoryHigh {
        /// Used percentage.
        percent: f64,
    },
    /// systemd units in the failed state.
    FailedUnits(Vec<String>),
}

impl ProbeFlag {
    /// Short text for the dashboard.
    pub fn label(&self) -> String {
        match self {
            ProbeFlag::DiskFull { mount, percent } => format!("{mount} {percent:.0}% full"),
            ProbeFlag::MemoryHigh { percent } => format!("memory {percent:.0}%"),
            ProbeFlag::FailedUnits(u) if u.len() == 1 => format!("failed: {}", u[0]),
            ProbeFlag::FailedUnits(u) => format!("{} failed units", u.len()),
        }
    }
}

impl ProbeReport {
    /// The fullest real file system.
    pub fn fullest_disk(&self) -> Option<&Disk> {
        self.disks
            .iter()
            .max_by(|a, b| a.use_percent.total_cmp(&b.use_percent))
    }

    /// What needs attention.
    pub fn flags(&self) -> Vec<ProbeFlag> {
        let mut out: Vec<ProbeFlag> = self
            .disks
            .iter()
            .filter(|d| d.use_percent >= FULL_PERCENT)
            .map(|d| ProbeFlag::DiskFull {
                mount: d.mount.clone(),
                percent: d.use_percent,
            })
            .collect();
        if let Some(m) = self.memory
            && m.used_percent() >= FULL_PERCENT
        {
            out.push(ProbeFlag::MemoryHigh {
                percent: m.used_percent(),
            });
        }
        if let Some(units) = &self.failed_units
            && !units.is_empty()
        {
            out.push(ProbeFlag::FailedUnits(units.clone()));
        }
        out
    }
}

/// Split probe output into its `@@name` sections.
fn sections(output: &str) -> Vec<(&str, String)> {
    let mut out: Vec<(&str, String)> = Vec::new();
    for line in output.lines() {
        if let Some(name) = line.trim_end().strip_prefix("@@")
            && !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && name != "nosystemd"
        {
            out.push((name, String::new()));
            continue;
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// Parse everything the probe printed.
pub fn parse_probe(output: &str) -> ProbeReport {
    let secs = sections(output);
    let get = |name: &str| {
        secs.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b.as_str())
            .unwrap_or("")
    };
    let (cmd_uptime, cmd_load) = parse_uptime(get("uptime"));
    let free = parse_free(get("free"));
    ProbeReport {
        uptime_secs: parse_proc_uptime(get("proc_uptime")).or(cmd_uptime),
        load: parse_loadavg(get("loadavg")).or(cmd_load),
        cpus: get("nproc").trim().parse().ok().filter(|n| *n > 0),
        disks: parse_df(get("df")),
        memory: parse_meminfo(get("meminfo")).or(free),
        failed_units: if output.contains("@@nosystemd") {
            None
        } else {
            Some(parse_failed_units(get("failed")))
        },
        os: parse_os_release(get("os")),
        kernel: Some(get("uname").trim().to_owned()).filter(|k| !k.is_empty()),
    }
}

/// `/proc/uptime`: seconds since boot.
pub fn parse_proc_uptime(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    first.parse::<f64>().ok().map(|s| s as u64)
}

/// `/proc/loadavg`: the three load averages.
pub fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut it = text.split_whitespace().map(|v| v.parse::<f64>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

/// The `uptime` command's line: time since boot and load averages. Handles procps
/// (`up 10 days,  2:03,  2 users,  load average: …`), BusyBox (`up 3 days,  4:05,
/// load average: …`), short uptimes (`up 5 min,`) and BSD/macOS (`load averages: a b c`).
pub fn parse_uptime(text: &str) -> (Option<u64>, Option<[f64; 3]>) {
    let line = text.lines().find(|l| l.contains(" up ")).unwrap_or("");
    let load = line.find("load average").and_then(|at| {
        let rest = &line[at..];
        let rest = rest.split_once(':').map(|(_, r)| r)?;
        let v: Vec<f64> = rest
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        (v.len() >= 3).then(|| [v[0], v[1], v[2]])
    });
    let up = line.split_once(" up ").map(|(_, r)| r).and_then(|rest| {
        let rest = rest.split("load average").next().unwrap_or(rest);
        let mut secs: u64 = 0;
        let mut any = false;
        for part in rest.split(',') {
            let p = part.trim();
            if p.contains("user") {
                break;
            }
            let mut words = p.split_whitespace();
            match (words.next(), words.next()) {
                (Some(n), Some(unit)) if unit.starts_with("day") => {
                    secs += n.parse::<u64>().ok()? * 86_400;
                    any = true;
                }
                (Some(n), Some(unit)) if unit.starts_with("min") => {
                    secs += n.parse::<u64>().ok()? * 60;
                    any = true;
                }
                (Some(n), Some(unit)) if unit.starts_with("sec") => {
                    secs += n.parse::<u64>().ok()?;
                    any = true;
                }
                (Some(hm), None) if hm.contains(':') => {
                    let (h, m) = hm.split_once(':')?;
                    secs += h.parse::<u64>().ok()? * 3600 + m.parse::<u64>().ok()? * 60;
                    any = true;
                }
                _ => {}
            }
        }
        any.then_some(secs)
    });
    (up, load)
}

/// Pseudo file systems the dashboard leaves out (always full, or not disks).
fn pseudo_fs(filesystem: &str, mount: &str) -> bool {
    const FS: &[&str] = &[
        "tmpfs", "devtmpfs", "udev", "overlay", "shm", "none", "squashfs", "efivarfs", "cgroup",
        "cgroup2", "proc", "sysfs", "devfs", "map",
    ];
    FS.contains(&filesystem)
        || filesystem.starts_with("/dev/loop")
        || mount.starts_with("/snap/")
        || mount.starts_with("/proc")
        || mount.starts_with("/sys")
        || mount == "/dev"
        || mount.starts_with("/dev/")
        || mount.starts_with("/run")
}

/// `df -P` (POSIX format, 1024-byte blocks): real file systems only.
pub fn parse_df(text: &str) -> Vec<Disk> {
    let mut out = Vec::new();
    for line in text
        .lines()
        .skip_while(|l| !l.starts_with("Filesystem"))
        .skip(1)
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        // The mount point is the rest of the line after the capacity column (it may
        // hold spaces); the file system the first field.
        let Some(cap_at) = f.iter().position(|w| w.ends_with('%')) else {
            continue;
        };
        if cap_at < 4 {
            continue;
        }
        let filesystem = f[..cap_at - 3].join(" ");
        let size_kb: u64 = f[cap_at - 3].parse().unwrap_or(0);
        let used_kb: u64 = f[cap_at - 2].parse().unwrap_or(0);
        let mount = f[cap_at + 1..].join(" ");
        if size_kb == 0 || mount.is_empty() || pseudo_fs(&filesystem, &mount) {
            continue;
        }
        let use_percent = f[cap_at]
            .trim_end_matches('%')
            .parse::<f64>()
            .unwrap_or_else(|_| used_kb as f64 * 100.0 / size_kb as f64);
        out.push(Disk {
            filesystem,
            mount,
            size_kb,
            used_kb,
            use_percent,
        });
    }
    out
}

/// `/proc/meminfo` (kB values).
pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == name).then(|| v.split_whitespace().next()?.parse::<u64>().ok())?
        })
    };
    let total = field("MemTotal")? * 1024;
    let available = match field("MemAvailable") {
        Some(a) => a * 1024,
        // Kernels before 3.14.
        None => {
            (field("MemFree").unwrap_or(0)
                + field("Buffers").unwrap_or(0)
                + field("Cached").unwrap_or(0))
                * 1024
        }
    };
    Some(Memory { total, available })
}

/// `free -b`: the `Mem:` line, read by the header's column names (procps 3.3+ has
/// `available`; older procps and BusyBox `buffers` / `cached`).
pub fn parse_free(text: &str) -> Option<Memory> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next()?.split_whitespace().collect();
    let mem = text.lines().find(|l| l.trim_start().starts_with("Mem:"))?;
    let vals: Vec<u64> = mem
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .and_then(|i| vals.get(i).copied())
    };
    let total = col("total")?;
    let available = col("available").unwrap_or_else(|| {
        col("free").unwrap_or(0)
            + col("buff/cache")
                .unwrap_or_else(|| col("buffers").unwrap_or(0) + col("cached").unwrap_or(0))
    });
    Some(Memory { total, available })
}

/// `systemctl --failed --no-legend --plain`: unit names.
pub fn parse_failed_units(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let first = words.next()?;
            // Without `--plain` (old systemd) a bullet comes first.
            let unit = if first == "●" || first == "*" {
                words.next()?
            } else {
                first
            };
            unit.contains('.').then(|| unit.to_owned())
        })
        .collect()
}

/// `/etc/os-release`: `PRETTY_NAME`, else `NAME VERSION_ID`.
pub fn parse_os_release(text: &str) -> Option<String> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == name).then(|| v.trim().trim_matches(['"', '\'']).to_owned())
        })
    };
    field("PRETTY_NAME").filter(|s| !s.is_empty()).or_else(|| {
        match (field("NAME"), field("VERSION_ID")) {
            (Some(n), Some(v)) => Some(format!("{n} {v}")),
            (Some(n), None) => Some(n),
            _ => None,
        }
    })
}

/// `3d 4h`, `5h 12m`, `7m`.
pub fn format_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const UBUNTU: &str = "@@uptime
 14:02:11 up 10 days,  2:03,  2 users,  load average: 0.08, 0.03, 0.01
@@proc_uptime
874993.27 3456789.12
@@loadavg
0.08 0.03 0.01 1/234 5678
@@nproc
4
@@df
Filesystem     1024-blocks     Used Available Capacity Mounted on
tmpfs               401996     1100    400896       1% /run
/dev/sda1         40581564 37200012   3365168      92% /
tmpfs              2009976        0   2009976       0% /dev/shm
/dev/loop0           65024    65024         0     100% /snap/core20/2105
/dev/sda15          106832     6186    100646       6% /boot/efi
//nas/share data 1000 500 500 50% /mnt/nas share
@@free
               total        used        free      shared  buff/cache   available
Mem:      4119531520   812345344  1234567168     1234567  2072619008  3001234432
Swap:              0           0           0
@@meminfo
MemTotal:        4022980 kB
MemFree:         1205632 kB
MemAvailable:    2930892 kB
Buffers:          102400 kB
Cached:          1800000 kB
@@failed
nginx.service loaded failed failed A high performance web server
snapd.seeded.service loaded failed failed Wait until snapd is fully seeded
@@os
PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"
NAME=\"Ubuntu\"
VERSION_ID=\"24.04\"
@@uname
Linux 6.8.0-45-generic
@@end
";

    const ALPINE: &str = "@@uptime
 12:00:00 up 3 days,  4:05,  load average: 0.00, 0.01, 0.05
@@proc_uptime
@@loadavg
@@nproc
2
@@df
Filesystem           1024-blocks    Used Available Capacity Mounted on
overlay                 61255492  9824740  48289552  17% /
tmpfs                      65536        0     65536   0% /dev
/dev/vda1               61255492  9824740  48289552  17% /etc/hosts
@@free
              total        used        free      shared  buff/cache   available
Mem:     2084044800   152342528  1500000000      102400   431702272  1856000000
Swap:             0           0           0
@@meminfo
@@failed
@@nosystemd
@@os
NAME=\"Alpine Linux\"
ID=alpine
VERSION_ID=3.20.3
PRETTY_NAME=\"Alpine Linux v3.20\"
@@uname
Linux 6.6.32-0-virt
@@end
";

    // RHEL 7 era: old procps `free` without `available`, no MemAvailable, systemd
    // without `--plain` (bullets), uptime under a day.
    const RHEL: &str = "@@uptime
 09:15:01 up 23 min,  1 user,  load average: 1.52, 0.80, 0.33
@@proc_uptime
@@loadavg
@@nproc
@@df
Filesystem              1024-blocks    Used Available Capacity Mounted on
/dev/mapper/rhel-root      17811456 3141592  14669864      18% /
/dev/sda1                   1038336  1000000     38336      97% /boot
@@free
             total       used       free     shared    buffers     cached
Mem:     1928466432 1850000000   78466432    8888888   10000000   20000000
-/+ buffers/cache:  1820000000  108466432
@@meminfo
MemTotal:        1883268 kB
MemFree:           76628 kB
Buffers:            9766 kB
Cached:            19532 kB
@@failed
● kdump.service loaded failed failed Crash recovery kernel arming
@@os
NAME=\"Red Hat Enterprise Linux Server\"
VERSION_ID=\"7.9\"
@@uname
Linux 3.10.0-1160.el7.x86_64
@@end
";

    #[test]
    fn ubuntu_probe() {
        let r = parse_probe(UBUNTU);
        assert_eq!(r.uptime_secs, Some(874_993));
        assert_eq!(r.load, Some([0.08, 0.03, 0.01]));
        assert_eq!(r.cpus, Some(4));
        let mounts: Vec<&str> = r.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/boot/efi", "/mnt/nas share"]);
        assert_eq!(r.disks[2].filesystem, "//nas/share data");
        assert_eq!(r.disks[0].use_percent, 92.0);
        assert_eq!(r.fullest_disk().map(|d| d.mount.as_str()), Some("/"));
        // /proc/meminfo wins over free.
        let m = r.memory.unwrap();
        assert_eq!(m.total, 4_022_980 * 1024);
        assert_eq!(m.available, 2_930_892 * 1024);
        assert_eq!(
            r.failed_units.as_deref(),
            Some(
                &[
                    "nginx.service".to_owned(),
                    "snapd.seeded.service".to_owned()
                ][..]
            )
        );
        assert_eq!(r.os.as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(r.kernel.as_deref(), Some("Linux 6.8.0-45-generic"));
        let flags = r.flags();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].label(), "/ 92% full");
        assert_eq!(flags[1].label(), "2 failed units");
    }

    #[test]
    fn alpine_busybox_probe() {
        let r = parse_probe(ALPINE);
        // No /proc/uptime or /proc/loadavg section output: from `uptime`.
        assert_eq!(r.uptime_secs, Some(3 * 86_400 + 4 * 3600 + 5 * 60));
        assert_eq!(r.load, Some([0.0, 0.01, 0.05]));
        assert_eq!(r.cpus, Some(2));
        // overlay and tmpfs are left out.
        assert_eq!(r.disks.len(), 1);
        assert_eq!(r.disks[0].mount, "/etc/hosts");
        let m = r.memory.unwrap();
        assert_eq!((m.total, m.available), (2_084_044_800, 1_856_000_000));
        assert_eq!(r.failed_units, None);
        assert_eq!(r.os.as_deref(), Some("Alpine Linux v3.20"));
        assert!(r.flags().is_empty());
    }

    #[test]
    fn rhel_probe() {
        let r = parse_probe(RHEL);
        assert_eq!(r.uptime_secs, Some(23 * 60));
        assert_eq!(r.load, Some([1.52, 0.80, 0.33]));
        assert_eq!(r.cpus, None);
        assert_eq!(r.disks.len(), 2);
        // MemAvailable missing: free + buffers + cached.
        let m = r.memory.unwrap();
        assert_eq!(m.available, (76_628 + 9_766 + 19_532) * 1024);
        assert!(m.used_percent() > 90.0);
        assert_eq!(r.failed_units, Some(vec!["kdump.service".to_owned()]));
        assert_eq!(r.os.as_deref(), Some("Red Hat Enterprise Linux Server 7.9"));
        let labels: Vec<String> = r.flags().iter().map(ProbeFlag::label).collect();
        assert_eq!(
            labels,
            ["/boot 97% full", "memory 94%", "failed: kdump.service"]
        );
    }

    #[test]
    fn free_without_available_column() {
        let text = "             total       used       free     shared    buffers     cached
Mem:          1000        900        100          0         50        150
";
        assert_eq!(
            parse_free(text),
            Some(Memory {
                total: 1000,
                available: 300
            })
        );
        assert_eq!(parse_free(""), None);
    }

    #[test]
    fn uptime_variants() {
        assert_eq!(
            parse_uptime(" 10:00  up 1 day, 23 min, 3 users, load averages: 1.10 1.20 1.30"),
            (Some(86_400 + 23 * 60), Some([1.10, 1.20, 1.30]))
        );
        assert_eq!(
            parse_uptime(" 10:00:00 up 0 min,  0 users,  load average: 0.00, 0.00, 0.00").0,
            Some(0)
        );
        assert_eq!(parse_uptime(""), (None, None));
        assert_eq!(format_uptime(3 * 86_400 + 7200), "3d 2h");
        assert_eq!(format_uptime(5 * 3600 + 120), "5h 2m");
        assert_eq!(format_uptime(59), "0m");
    }

    #[test]
    fn empty_output_parses_to_nothing() {
        let r = parse_probe("");
        assert_eq!(
            r,
            ProbeReport {
                failed_units: Some(Vec::new()),
                ..ProbeReport::default()
            }
        );
        assert!(r.flags().is_empty());
    }
}
