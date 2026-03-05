use anyhow::Context;
use gossip_core::bench_utils;
use gossip_core::protocols::tryst::crypto::EpochSecret;
use hdrhistogram::Histogram;
use serde::Deserialize;
use std::fs::File;
use std::process::{Command, Stdio};

// --- Config Structs ---

#[derive(Deserialize)]
struct BenchConfig {
    crypto: Option<CryptoConfig>,
    discovery: Option<DiscoveryConfig>,
    latency: Option<LatencyConfig>,
}

#[derive(Deserialize)]
struct CryptoConfig {
    epochs_to_simulate: Vec<u64>,
}

#[derive(Deserialize)]
struct DiscoveryConfig {
    iterations: usize,
}

#[derive(Deserialize)]
struct LatencyConfig {
    iterations: usize,
}

// --- Benchmark Scenarios ---

fn measure_crypto(config: &CryptoConfig) -> Vec<String> {
    let mut results = Vec::new();
    println!("--- Hardware-Agnostic Cryptographic Cost ---");
    for &epochs in &config.epochs_to_simulate {
        let mut secret = EpochSecret::generate_random();
        let iters = 1000;
        let start_cpu = bench_utils::get_cpu_time_us();
        for _ in 0..iters {
            for _ in 0..epochs {
                secret.ratchet_secret();
            }
        }
        let end_cpu = bench_utils::get_cpu_time_us();
        let elapsed_us = (end_cpu.saturating_sub(start_cpu)) / iters;
        let elapsed_ms = elapsed_us as f64 / 1000.0;
        let row = format!(
            "crypto_cost,epochs,{},cpu_time_ms,{:.6}",
            epochs, elapsed_ms
        );
        println!("{}", row);
        results.push(row);
    }
    results
}

fn report_hist(
    name: &str,
    metric: &str,
    hist: &Histogram<u64>,
    scale: f64,
    unit: &str,
) -> Vec<String> {
    let mut results = Vec::new();
    if hist.len() > 0 {
        let quantiles = [0.50, 0.75, 0.90, 0.95, 0.99];
        let mut row_parts = Vec::new();

        for q in quantiles {
            let val = hist.value_at_quantile(q) as f64 * scale;
            let q_str = (q * 100.0) as u64;
            results.push(format!("{name},p{q_str}_{metric},{val:.2}"));
            row_parts.push(format!("p{q_str}: {val:.2} {unit}"));
        }
        println!("{:<15} - {}", name, row_parts.join(", "));
    }
    results
}

async fn run_worker(bin: &str, args: &[String]) -> anyhow::Result<Vec<u64>> {
    let exe = std::env::current_exe()?;
    let parent_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent dir"))?;
    let worker_exe = parent_dir.join(bin);

    let child = Command::new(&worker_exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        if line.starts_with("RESULT,") {
            return line[7..]
                .split(',')
                .map(|s| s.parse::<u64>().map_err(anyhow::Error::from))
                .collect();
        }
    }
    anyhow::bail!("Worker failed to return result")
}

async fn measure_dht_discovery(iterations: usize) -> anyhow::Result<Vec<String>> {
    println!("--- Pure DHT Discovery Costs (Publish & Resolve) ---");
    let mut pub_bytes_hist = Histogram::<u64>::new(3)?;
    let mut pub_cpu_hist = Histogram::<u64>::new(3)?;
    let mut res_latency_hist = Histogram::<u64>::new(3)?;
    let mut res_bytes_hist = Histogram::<u64>::new(3)?;
    let mut res_cpu_hist = Histogram::<u64>::new(3)?;

    for i in 1..=iterations {
        println!("\nRunning DHT Discovery Iteration {}/{}...", i, iterations);
        let bob_sk = gossip_core::app::generate_private_key();
        let alice_sk = gossip_core::app::generate_private_key();

        match run_worker(
            "bench_tryst_discovery_worker",
            &[i.to_string(), bob_sk, alice_sk],
        )
        .await
        {
            Ok(parts) if parts.len() == 5 => {
                pub_bytes_hist.record(parts[0])?;
                pub_cpu_hist.record(parts[1])?;
                res_latency_hist.record(parts[2])?;
                res_bytes_hist.record(parts[3])?;
                res_cpu_hist.record(parts[4])?;
                println!(
                    "  Success: pub: {} B, res: {} B, {} ms",
                    parts[0], parts[3], parts[2]
                );
            }
            _ => println!("  Worker reported an error."),
        }
    }

    let mut results = Vec::new();
    println!("\n--- DHT Discovery Results ---");
    results.extend(report_hist(
        "dht_publish",
        "bytes",
        &pub_bytes_hist,
        1.0,
        "B",
    ));
    results.extend(report_hist(
        "dht_publish",
        "cpu_ms",
        &pub_cpu_hist,
        0.001,
        "ms",
    ));
    results.extend(report_hist(
        "dht_resolve",
        "latency_ms",
        &res_latency_hist,
        1.0,
        "ms",
    ));
    results.extend(report_hist(
        "dht_resolve",
        "bytes",
        &res_bytes_hist,
        1.0,
        "B",
    ));
    results.extend(report_hist(
        "dht_resolve",
        "cpu_ms",
        &res_cpu_hist,
        0.001,
        "ms",
    ));
    Ok(results)
}

async fn measure_connection_establishment(
    iterations: usize,
) -> anyhow::Result<Vec<String>> {
    println!("--- Full Connection Establishment Costs ---");
    let mut latency_hist = Histogram::<u64>::new(3)?;
    let mut bytes_hist = Histogram::<u64>::new(3)?;
    let mut cpu_hist = Histogram::<u64>::new(3)?;

    for i in 1..=iterations {
        println!(
            "\nRunning Connection Establishment Iteration {}/{}...",
            i, iterations
        );
        let bob_sk = gossip_core::app::generate_private_key();
        let alice_sk = gossip_core::app::generate_private_key();

        match run_worker("bench_p2p_conn_worker", &[i.to_string(), bob_sk, alice_sk])
            .await
        {
            Ok(parts) if parts.len() == 3 => {
                latency_hist.record(parts[0])?;
                bytes_hist.record(parts[1])?;
                cpu_hist.record(parts[2])?;
                println!("  Success: {} ms, {} B", parts[0], parts[1]);
            }
            _ => println!("  Worker reported an error."),
        }
    }

    let mut results = Vec::new();
    println!("\n--- Connection Establishment Results ---");
    results.extend(report_hist(
        "conn_est",
        "latency_ms",
        &latency_hist,
        1.0,
        "ms",
    ));
    results.extend(report_hist("conn_est", "bytes", &bytes_hist, 1.0, "B"));
    results.extend(report_hist("conn_est", "cpu_ms", &cpu_hist, 0.001, "ms"));
    Ok(results)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let possible_paths = [
        "bench_config.ron",
        "./config/bench_config.ron",
        "../bench_config.ron",
    ];

    let mut config_path = None;
    for path in possible_paths {
        if std::path::Path::new(path).exists() {
            config_path = Some(std::path::PathBuf::from(path));
            break;
        }
    }

    // Fallback to executable directory
    if config_path.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let alt_path = parent.join("bench_config.ron");
                if alt_path.exists() {
                    config_path = Some(alt_path);
                }
            }
        }
    }

    let config_path = config_path.ok_or_else(|| {
        anyhow::anyhow!(
            "Configuration file 'bench_config.ron' not found in root, ./config, or executable directory."
        )
    })?;

    let file = File::open(&config_path)
        .with_context(|| format!("Failed to open config file at {:?}", config_path))?;
    let config: BenchConfig = ron::de::from_reader(file)?;

    let mut all_results = Vec::new();
    all_results.push("run_type,metric,param,value".to_string());

    if let Some(crypto) = &config.crypto {
        all_results.extend(measure_crypto(crypto));
    }
    if let Some(discovery) = &config.discovery {
        all_results.extend(measure_dht_discovery(discovery.iterations).await?);
    }
    if let Some(latency) = &config.latency {
        all_results
            .extend(measure_connection_establishment(latency.iterations).await?);
    }

    std::fs::write("bench_results.csv", all_results.join("\n") + "\n")?;
    println!("\n✅ Benchmarks complete! Results written to bench_results.csv");
    Ok(())
}
