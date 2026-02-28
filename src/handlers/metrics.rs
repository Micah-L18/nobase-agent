//! System metrics collection handler.
//!
//! Collects CPU, memory, disk, network, GPU, and Docker metrics
//! using the `sysinfo` crate — replaces the 17+ sequential SSH commands
//! used by the Node.js backend collector.

use agent_proto::*;
use sysinfo::{Disks, Networks, System};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::debug;

/// Holds the previous network snapshot for rate calculation.
struct NetworkSnapshot {
    rx_total: u64,
    tx_total: u64,
    timestamp: Instant,
}

static PREV_NET_SNAPSHOT: Mutex<Option<NetworkSnapshot>> = Mutex::new(None);

/// Cached memory speed — hardware constant, only needs one dmidecode call ever.
static MEMORY_SPEED_CACHE: OnceLock<Option<u64>> = OnceLock::new();

/// Cached GPU metrics with timestamp for 30-second TTL.
static GPU_CACHE: Mutex<Option<(Instant, Option<GpuMetrics>)>> = Mutex::new(None);

/// Cached Docker metrics with timestamp for 30-second TTL.
static DOCKER_CACHE: Mutex<Option<(Instant, Option<DockerMetrics>)>> = Mutex::new(None);

const PROBE_CACHE_TTL: Duration = Duration::from_secs(30);

/// Handle an on-demand metrics.collect RPC request.
/// Creates a fresh System instance, refreshes twice (1s delay for CPU usage), returns metrics.
pub async fn handle_metrics_collect(id: String) -> RpcResponse {
    debug!("metrics.collect");

    let metrics = collect_metrics_once().await;

    RpcResponse::success(
        id,
        serde_json::to_value(metrics).unwrap_or_default(),
    )
}

/// Collect a full metrics snapshot (standalone, with 1s delay for CPU usage).
pub async fn collect_metrics_once() -> MetricsPayload {
    let mut sys = System::new();

    // First CPU refresh (establishes baseline)
    sys.refresh_cpu_all();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Second refresh for accurate CPU usage
    sys.refresh_cpu_all();
    sys.refresh_memory();

    build_metrics(&sys)
}

/// Collect metrics using an existing, already-refreshed System instance.
/// Used by the periodic push task which keeps a persistent System across iterations.
pub fn collect_metrics_from_system(sys: &System) -> MetricsPayload {
    build_metrics(sys)
}

/// Build a MetricsPayload from a refreshed System.
fn build_metrics(sys: &System) -> MetricsPayload {
    // ─── Hostname & OS ──────────────────────────────────────────────────
    let hostname = System::host_name().unwrap_or_else(|| "unknown".into());
    let os_name = System::name().unwrap_or_else(|| std::env::consts::OS.into());
    let os_version = System::os_version().unwrap_or_else(|| "unknown".into());
    let kernel = System::kernel_version().unwrap_or_else(|| "unknown".into());
    let arch = std::env::consts::ARCH.to_string();
    let uptime_secs = System::uptime();

    // ─── CPU ────────────────────────────────────────────────────────────
    let cpus = sys.cpus();
    let cpu_model = cpus.first().map(|c| c.brand().to_string()).unwrap_or_default();
    let cpu_cores = cpus.len() as u32;
    let cpu_usage = sys.global_cpu_usage() as f64;
    let cpu_mhz = cpus.first().map(|c| c.frequency() as f64).unwrap_or(0.0);

    // Try to get CPU temperature from thermal zones
    let cpu_temp = get_cpu_temperature();

    // ─── Memory ─────────────────────────────────────────────────────────
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let available_mem = sys.available_memory();
    let total_swap = sys.total_swap();
    let used_swap = sys.used_swap();

    // Try to get memory speed
    let mem_speed = get_memory_speed();

    // ─── Disks ──────────────────────────────────────────────────────────
    let disk_list = Disks::new_with_refreshed_list();
    let disks: Vec<DiskMetrics> = disk_list
        .iter()
        .filter(|d| {
            // Filter out pseudo-filesystems
            let fs = d.file_system().to_string_lossy();
            !matches!(
                fs.as_ref(),
                "tmpfs" | "devtmpfs" | "overlay" | "squashfs" | "efivarfs"
            )
        })
        .map(|d| {
            let total = d.total_space() as f64 / (1024.0 * 1024.0 * 1024.0);
            let available = d.available_space() as f64 / (1024.0 * 1024.0 * 1024.0);
            let used = total - available;
            let usage_percent = if total > 0.0 { (used / total) * 100.0 } else { 0.0 };

            DiskMetrics {
                mount: d.mount_point().to_string_lossy().to_string(),
                filesystem: d.file_system().to_string_lossy().to_string(),
                total_gb: (total * 10.0).round() / 10.0,
                used_gb: (used * 10.0).round() / 10.0,
                available_gb: (available * 10.0).round() / 10.0,
                usage_percent: (usage_percent * 10.0).round() / 10.0,
                device: d.name().to_string_lossy().to_string(),
            }
        })
        .collect();

    // ─── Network ────────────────────────────────────────────────────────
    let networks = Networks::new_with_refreshed_list();

    // Find the primary interface (highest traffic, excluding lo/veth/docker/br-)
    let mut primary_iface = String::new();
    let mut max_traffic: u64 = 0;
    let mut rx_total: u64 = 0;
    let mut tx_total: u64 = 0;

    for (name, data) in networks.iter() {
        if name.starts_with("lo") || name.starts_with("veth") 
            || name.starts_with("docker") || name.starts_with("br-") {
            continue;
        }
        let iface_rx = data.total_received();
        let iface_tx = data.total_transmitted();
        rx_total += iface_rx;
        tx_total += iface_tx;
        let traffic = iface_rx + iface_tx;
        if traffic > max_traffic {
            max_traffic = traffic;
            primary_iface = name.clone();
        }
    }

    // Compute per-second rates using the previous snapshot
    let now = Instant::now();
    let (rx_rate, tx_rate) = {
        let mut prev = PREV_NET_SNAPSHOT.lock().unwrap();
        let rates = if let Some(ref snapshot) = *prev {
            let elapsed = now.duration_since(snapshot.timestamp).as_secs_f64();
            if elapsed > 0.1 {
                let rx_delta = rx_total.saturating_sub(snapshot.rx_total);
                let tx_delta = tx_total.saturating_sub(snapshot.tx_total);
                ((rx_delta as f64 / elapsed) as u64, (tx_delta as f64 / elapsed) as u64)
            } else {
                (0u64, 0u64)
            }
        } else {
            (0u64, 0u64) // First collection, no rate yet
        };
        *prev = Some(NetworkSnapshot {
            rx_total,
            tx_total,
            timestamp: now,
        });
        rates
    };

    // ─── GPU (nvidia-smi) ───────────────────────────────────────────────
    let gpu = get_gpu_metrics();

    // ─── Docker ─────────────────────────────────────────────────────────
    let docker = get_docker_metrics();

    MetricsPayload {
        hostname,
        os: OsInfo {
            name: os_name,
            version: os_version,
            kernel,
            arch,
        },
        uptime_secs,
        cpu: CpuMetrics {
            model: cpu_model,
            cores: cpu_cores,
            usage_percent: (cpu_usage * 10.0).round() / 10.0,
            temperature_celsius: cpu_temp,
            mhz: cpu_mhz,
        },
        memory: MemoryMetrics {
            total_mb: total_mem / (1024 * 1024),
            used_mb: used_mem / (1024 * 1024),
            available_mb: available_mem / (1024 * 1024),
            swap_total_mb: total_swap / (1024 * 1024),
            swap_used_mb: used_swap / (1024 * 1024),
            speed_mhz: mem_speed,
        },
        disks,
        network: NetworkMetrics {
            interface: primary_iface,
            rx_bytes_per_sec: rx_rate,
            tx_bytes_per_sec: tx_rate,
            rx_total_bytes: rx_total,
            tx_total_bytes: tx_total,
            latency_ms: 0.0, // Measured async if needed
        },
        gpu,
        docker,
    }
}

/// Read CPU temperature from thermal zones (Linux).
fn get_cpu_temperature() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        // Try /sys/class/thermal/thermal_zone0/temp
        if let Ok(content) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
            if let Ok(millideg) = content.trim().parse::<f64>() {
                return Some((millideg / 1000.0 * 10.0).round() / 10.0);
            }
        }
    }
    None
}

/// Try to get memory speed (MHz) from dmidecode or sysfs.
/// Cached permanently via OnceLock — RAM speed never changes at runtime.
fn get_memory_speed() -> Option<u64> {
    *MEMORY_SPEED_CACHE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            // Try dmidecode (requires root)
            if let Ok(output) = std::process::Command::new("sudo")
                .args(["dmidecode", "-t", "memory"])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.contains("Speed:") && !line.contains("Unknown") {
                        if let Some(speed) = line.split_whitespace().nth(1) {
                            if let Ok(mhz) = speed.parse::<u64>() {
                                return Some(mhz);
                            }
                        }
                    }
                }
            }
        }
        None
    })
}

/// Probe GPU metrics via nvidia-smi.
/// Cached for 30 seconds — GPU utilization changes, but sampling every second is wasteful.
fn get_gpu_metrics() -> Option<GpuMetrics> {
    {
        let cache = GPU_CACHE.lock().unwrap();
        if let Some((ts, ref cached)) = *cache {
            if ts.elapsed() < PROBE_CACHE_TTL {
                return cached.clone();
            }
        }
    }

    let result = probe_gpu_metrics();
    *GPU_CACHE.lock().unwrap() = Some((Instant::now(), result.clone()));
    result
}

fn probe_gpu_metrics() -> Option<GpuMetrics> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=gpu_name,memory.total,memory.used,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next()?.trim();
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 5 {
        return None;
    }

    Some(GpuMetrics {
        name: parts[0].to_string(),
        memory_total_mb: parts[1].parse().unwrap_or(0),
        memory_used_mb: parts[2].parse().unwrap_or(0),
        usage_percent: parts[3].parse().unwrap_or(0.0),
        temperature_celsius: parts[4].parse().unwrap_or(0.0),
    })
}

/// Check Docker daemon status.
/// Cached for 30 seconds — container counts change infrequently.
fn get_docker_metrics() -> Option<DockerMetrics> {
    {
        let cache = DOCKER_CACHE.lock().unwrap();
        if let Some((ts, ref cached)) = *cache {
            if ts.elapsed() < PROBE_CACHE_TTL {
                return cached.clone();
            }
        }
    }

    let result = probe_docker_metrics();
    *DOCKER_CACHE.lock().unwrap() = Some((Instant::now(), result.clone()));
    result
}

fn probe_docker_metrics() -> Option<DockerMetrics> {
    let output = std::process::Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}|{{.Containers}}|{{.ContainersRunning}}")
        .output()
        .ok()?;

    if !output.status.success() {
        // Try with sudo
        let output = std::process::Command::new("sudo")
            .args(["docker", "info", "--format", "{{.ServerVersion}}|{{.Containers}}|{{.ContainersRunning}}"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = stdout.trim().split('|').collect();
        if parts.len() < 3 {
            return None;
        }

        return Some(DockerMetrics {
            installed: true,
            version: parts[0].to_string(),
            containers_total: parts[1].parse().unwrap_or(0),
            containers_running: parts[2].parse().unwrap_or(0),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().split('|').collect();
    if parts.len() < 3 {
        return None;
    }

    Some(DockerMetrics {
        installed: true,
        version: parts[0].to_string(),
        containers_total: parts[1].parse().unwrap_or(0),
        containers_running: parts[2].parse().unwrap_or(0),
    })
}
