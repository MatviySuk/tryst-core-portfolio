use gossip_core::app::{self, setup_db};
use gossip_core::bench_utils;
use gossip_core::protocols::tryst::{
    self, invite::InviteManager, persistence::TrystRepository,
};
use iroh::TransportAddr;
use iroh::discovery::Discovery;
use n0_future::stream::StreamExt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

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

    let db_root = bench_utils::setup_bench_db("dht", iteration).await?;

    // Setup DBs for both peers
    let bob_db_pool =
        setup_db(&format!("{}/bob.sqlite", db_root.to_string_lossy())).await?;
    let alice_db_pool =
        setup_db(&format!("{}/alice.sqlite", db_root.to_string_lossy())).await?;

    let bob_sk = app::secret_key_from_hex_lower(bob_sk_str)?;
    let alice_sk = app::secret_key_from_hex_lower(alice_sk_str)?;

    let bob_tryst_repo = TrystRepository::new(bob_db_pool);
    let bob_invite_manager =
        InviteManager::new(bob_sk.clone(), bob_tryst_repo.clone());

    // Bob creates an invite for Alice
    let tryst_invite_to_alice = bob_invite_manager
        .create_invite_str(tryst::invite::CreateInviteType::Sealed {
            recipient_pk: alice_sk.public(),
        })
        .await?;

    let bob_discovery =
        tryst::discovery::Builder::new(bob_sk.clone(), bob_tryst_repo)
            .build()
            .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let bob_endpoint_addr = iroh::EndpointAddr::from_parts(
        bob_sk.public(),
        vec![TransportAddr::Ip(
            SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0u16).into(),
        )],
    );
    let bob_info =
        iroh::discovery::EndpointData::new(bob_endpoint_addr.addrs.iter().cloned());

    // --- Measure Publish ---
    let (start_in, start_out) = bench_utils::get_bandwidth_bytes(pid);
    let start_cpu = bench_utils::get_cpu_time_us();

    bob_discovery.publish(&bob_info);

    let max_publish_bytes = bench_utils::wait_for_bandwidth_stabilization(pid).await;
    let end_cpu = bench_utils::get_cpu_time_us();

    let pub_bytes = max_publish_bytes.saturating_sub(start_in + start_out);
    let pub_cpu = end_cpu.saturating_sub(start_cpu);

    // --- Measure Resolve ---
    let alice_tryst_repo = TrystRepository::new(alice_db_pool);
    let alice_invite_manager =
        InviteManager::new(alice_sk.clone(), alice_tryst_repo.clone());
    alice_invite_manager
        .receive_invite_str(&tryst_invite_to_alice)
        .await?;

    let alice_discovery =
        tryst::discovery::Builder::new(alice_sk.clone(), alice_tryst_repo)
            .build()
            .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let (start_in, start_out) = bench_utils::get_bandwidth_bytes(pid);
    let start_cpu = bench_utils::get_cpu_time_us();
    let start_resolve = Instant::now();

    let mut res_latency = 0;
    let mut res_bytes = 0;
    let mut res_cpu = 0;
    let mut success = false;

    // Retry resolve for up to 10 seconds (standard DHT propagation)
    for _ in 0..10 {
        if let Some(mut stream) = alice_discovery.resolve(bob_sk.public()) {
            if stream.next().await.is_some() {
                res_latency = start_resolve.elapsed().as_millis() as u64;
                success = true;

                let max_resolve_bytes =
                    bench_utils::wait_for_bandwidth_stabilization(pid).await;
                let end_cpu = bench_utils::get_cpu_time_us();

                res_bytes = max_resolve_bytes.saturating_sub(start_in + start_out);
                res_cpu = end_cpu.saturating_sub(start_cpu);
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    if success {
        bench_utils::report_result(&[
            pub_bytes,
            pub_cpu,
            res_latency,
            res_bytes,
            res_cpu,
        ]);
    } else {
        println!("ERROR");
    }

    Ok(())
}
