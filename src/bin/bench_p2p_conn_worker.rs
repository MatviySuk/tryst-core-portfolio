use gossip_core::app::{Application, CreateInviteType};
use gossip_core::bench_utils;
use gossip_core::manager::ChatManagerReceiver;
use gossip_core::persistence::ChatMessage;
use gossip_core::protocols::chat::UniffiChatConnectionState;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct BenchReceiver {
    connected_tx: tokio::sync::Notify,
}

impl ChatManagerReceiver for BenchReceiver {
    fn process_messages(&self, _msgs: Vec<ChatMessage>) {}
    fn process_connection_state(&self, state: UniffiChatConnectionState) {
        if let UniffiChatConnectionState::Connected { .. } = state {
            self.connected_tx.notify_one();
        }
    }
    fn process_sync_status(&self, _is_synced: bool) {}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bench_utils::init_worker_tracing();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        anyhow::bail!("Usage: {} <iteration> <bob_sk_hex> <alice_sk_hex>", args[0]);
    }

    let iteration = &args[1];
    let bob_sk_str = &args[2];
    let alice_sk_str = &args[3];
    let pid = std::process::id();

    let db_root = bench_utils::setup_bench_db("conn", iteration).await?;
    let db_path = db_root.to_string_lossy().to_string();

    // Setup Alice and Bob Applications
    let alice = Application::new_with_config(
        alice_sk_str.to_string(),
        db_path.clone(),
        Duration::from_secs(2), // Active republisher
    ).await.map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let bob = Application::new_with_config(
        bob_sk_str.to_string(),
        db_path,
        Duration::from_secs(3600), // Silent
    ).await.map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Create an invite from Alice to Bob
    let invite_str = alice
        .create_tryst_invite(CreateInviteType::Sealed {
            recipient_pk: bob.endpoint_id(),
        })
        .await
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Wait for DHT propagation (since Alice just published her info)
    println!("Waiting for DHT propagation...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let (start_in, start_out) = bench_utils::get_bandwidth_bytes(pid);
    let start_cpu = bench_utils::get_cpu_time_us();
    let start_total = Instant::now();

    // Bob receives the invite and connects to Alice
    bob.receive_tryst_invite(invite_str)
        .await
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let bob_manager = bob
        .chat_manager(alice.endpoint_id())
        .await
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
    
    let receiver = Arc::new(BenchReceiver {
        connected_tx: tokio::sync::Notify::new(),
    });
    
    bob_manager
        .subscribe(receiver.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let mut success = false;
    let mut conn_latency = 0;

    // Timeout after 30 seconds for connection
    if tokio::time::timeout(Duration::from_secs(30), receiver.connected_tx.notified()).await.is_ok() {
        conn_latency = start_total.elapsed().as_millis() as u64;
        success = true;
    }

    // Wait for bandwidth to settle
    let max_conn_bytes = bench_utils::wait_for_bandwidth_stabilization(pid).await;
    let end_cpu = bench_utils::get_cpu_time_us();

    let conn_bytes = max_conn_bytes.saturating_sub(start_in + start_out);
    let conn_cpu = end_cpu.saturating_sub(start_cpu);

    alice.shutdown().await;
    bob.shutdown().await;

    if success {
        bench_utils::report_result(&[conn_latency, conn_bytes, conn_cpu]);
    } else {
        println!("ERROR");
    }

    Ok(())
}
