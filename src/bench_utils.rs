use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::prelude::*;

/// Returns total CPU time (user + system) in microseconds.
pub fn get_cpu_time_us() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        let usage = usage.assume_init();
        let user_cpu =
            usage.ru_utime.tv_sec as u64 * 1_000_000 + usage.ru_utime.tv_usec as u64;
        let sys_cpu =
            usage.ru_stime.tv_sec as u64 * 1_000_000 + usage.ru_stime.tv_usec as u64;
        user_cpu + sys_cpu
    }
}

#[cfg(target_os = "macos")]
pub fn get_bandwidth_bytes(pid: u32) -> (u64, u64) {
    use std::process::Command;
    let output = Command::new("nettop")
        .args(&["-l", "1", "-x", "-J", "bytes_in,bytes_out"])
        .output()
        .expect("failed to execute nettop");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid_suffix = format!(".{}", pid);

    for line in stdout.lines() {
        if line.contains("bytes_in") {
            continue;
        }

        let mut parts = line.split_whitespace();
        if let Some(name_pid) = parts.next() {
            if name_pid.ends_with(&pid_suffix) {
                let bytes_in =
                    parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
                let bytes_out =
                    parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
                return (bytes_in, bytes_out);
            }
        }
    }
    (0, 0)
}

#[cfg(target_os = "linux")]
pub fn get_bandwidth_bytes(_pid: u32) -> (u64, u64) {
    use std::fs;
    // In a Docker container, eth0 is typically the primary interface.
    let rx_str = fs::read_to_string("/sys/class/net/eth0/statistics/rx_bytes")
        .unwrap_or_else(|_| "0".to_string());
    let tx_str = fs::read_to_string("/sys/class/net/eth0/statistics/tx_bytes")
        .unwrap_or_else(|_| "0".to_string());

    let bytes_in: u64 = rx_str.trim().parse().unwrap_or(0);
    let bytes_out: u64 = tx_str.trim().parse().unwrap_or(0);

    (bytes_in, bytes_out)
}

/// Waits for bandwidth usage to stabilize (silence for 1.5s).
pub async fn wait_for_bandwidth_stabilization(pid: u32) -> u64 {
    let mut max_bytes = 0;
    let mut previous_total = 0;
    let mut silent_intervals = 0;

    for _ in 0..20 {
        // Max wait 10s
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (curr_in, curr_out) = get_bandwidth_bytes(pid);
        let current_total = curr_in + curr_out;

        max_bytes = max_bytes.max(current_total);
        let delta = current_total.saturating_sub(previous_total);

        if delta == 0 {
            silent_intervals += 1;
            if silent_intervals >= 3 {
                break;
            }
        } else {
            silent_intervals = 0;
        }
        previous_total = current_total;
    }

    max_bytes
}

/// Helper to setup a clean temporary database directory for workers.
pub async fn setup_bench_db(name: &str, iteration: &str) -> anyhow::Result<PathBuf> {
    let db_root =
        PathBuf::from(format!("./target/bench_dbs/{}_{}", name, iteration));
    if db_root.exists() {
        tokio::fs::remove_dir_all(&db_root).await?;
    }
    tokio::fs::create_dir_all(&db_root).await?;
    Ok(db_root)
}

/// Helper to initialize tracing for benchmark workers.
pub fn init_worker_tracing() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);
    let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
}

/// Unified result reporting for workers to communicate back to the orchestrator.
pub fn report_result(parts: &[u64]) {
    let result_str = parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    println!("RESULT,{}", result_str);
}
