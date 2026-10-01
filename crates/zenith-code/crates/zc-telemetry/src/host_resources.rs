//! `resourceTelemetry/HostResources.ts` (`server.getHostResources`): whole-host CPU use over a
//! 200 ms window and available memory, cached 5 s for every socket (concurrent reads share
//! one sample).

use std::sync::Arc;
use std::time::{Duration, Instant};

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tokio::sync::Mutex;
use zc_contracts::{DateTimeUtc, HostResourcesSnapshot, JsNumber};

const CPU_WINDOW: Duration = Duration::from_millis(200);
const TIME_TO_LIVE: Duration = Duration::from_secs(5);

/// `darwinAvailableMemory`: free + inactive + speculative pages of `vm_stat` (it prints free
/// pages without the speculative ones; purgeable pages overlap and are not added).
pub fn darwin_available_memory(output: &str) -> Option<i64> {
    let page_size: i64 = output.split("page size of ").nth(1)?.split(" bytes").next()?.trim().parse().ok()?;
    let pages = |label: &str| -> Option<i64> {
        output.lines().find_map(|line| {
            let rest = line.strip_prefix(label)?.trim_start();
            rest.strip_suffix('.')?.trim().parse().ok()
        })
    };
    let free = pages("Pages free:")?;
    let inactive = pages("Pages inactive:")?;
    let speculative = pages("Pages speculative:")?;
    if page_size <= 0 {
        return None;
    }
    (free + inactive + speculative).checked_mul(page_size).filter(|v| *v <= (1i64 << 53))
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
pub fn linux_available_memory(meminfo: &str) -> Option<i64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?.trim();
        let kb = rest.strip_suffix("kB")?.trim();
        kb.parse::<i64>().ok().map(|kb| kb * 1024)
    })
}

async fn available_memory(fallback: i64) -> i64 {
    if cfg!(target_os = "linux") {
        if let Ok(meminfo) = tokio::fs::read_to_string("/proc/meminfo").await {
            if let Some(available) = linux_available_memory(&meminfo) {
                return available;
            }
        }
    } else if cfg!(target_os = "macos") {
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::process::Command::new("/usr/bin/vm_stat")
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = output {
            if let Some(available) = darwin_available_memory(&String::from_utf8_lossy(&output.stdout)) {
                return available;
            }
        }
    }
    fallback
}

/// One sample (takes ≈ 200 ms).
pub async fn sample() -> HostResourcesSnapshot {
    let (cpu_utilization, cpu_count, total_memory, free_memory) = {
        let refresh = RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::nothing().with_ram());
        let mut system = System::new_with_specifics(refresh);
        let before = system.cpus().len();
        tokio::time::sleep(CPU_WINDOW.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)).await;
        system.refresh_cpu_usage();
        let count = system.cpus().len();
        let usage = f64::from(system.global_cpu_usage());
        let utilization = (before == count && usage.is_finite() && count > 0).then(|| (usage / 100.0).clamp(0.0, 1.0));
        (utilization, count as i64, system.total_memory() as i64, system.free_memory() as i64)
    };
    let available = available_memory(free_memory).await;
    HostResourcesSnapshot {
        sampled_at: DateTimeUtc::now().as_millis(),
        cpu_utilization: cpu_utilization.map(JsNumber),
        cpu_count,
        available_memory_bytes: available.max(0).min(total_memory),
        total_memory_bytes: total_memory,
    }
}

/// The cached reader.
#[derive(Clone, Default)]
pub struct HostResources {
    cache: Arc<Mutex<Option<(Instant, HostResourcesSnapshot)>>>,
}

impl HostResources {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn read(&self) -> HostResourcesSnapshot {
        let mut cache = self.cache.lock().await;
        if let Some((at, snapshot)) = cache.as_ref() {
            if at.elapsed() < TIME_TO_LIVE {
                return snapshot.clone();
            }
        }
        let snapshot = sample().await;
        *cache = Some((Instant::now(), snapshot.clone()));
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_vm_stat_pages() {
        let output = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               10.\nPages active:                            99.\nPages inactive:                           20.\nPages speculative:                         5.\n";
        assert_eq!(darwin_available_memory(output), Some(35 * 16_384));
        assert_eq!(darwin_available_memory("Pages free: 1."), None);
    }

    #[test]
    fn reads_meminfo() {
        assert_eq!(linux_available_memory("MemTotal:  100 kB\nMemAvailable:    2048 kB\n"), Some(2048 * 1024));
        assert_eq!(linux_available_memory("MemTotal: 100 kB\n"), None);
    }

    #[tokio::test]
    async fn samples_the_host_and_caches_the_result() {
        let host = HostResources::new();
        let first = host.read().await;
        assert!(first.cpu_count > 0);
        assert!(first.total_memory_bytes > 0);
        assert!(first.available_memory_bytes <= first.total_memory_bytes);
        assert!(first.cpu_utilization.is_none_or(|u| (0.0..=1.0).contains(&u.0)));
        assert_eq!(host.read().await, first);
    }
}
