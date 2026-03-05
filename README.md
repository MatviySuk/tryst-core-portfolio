# Tryst Core: Metadata-Resistant P2P Communication Engine

Tryst Core is a high-performance, local-first peer-to-peer (P2P) communication engine written in Rust. It serves as the backbone for decentralized messaging applications, providing robust identity management, CRDT-based data synchronization, and privacy-preserving peer discovery.

This repository focuses on the **Rust Core**, designed to be embedded into mobile (iOS/Android) and desktop applications via UniFFI.

## 🚀 Key Features

- **Local-First Architecture:** Users physically own their data. All messages are stored in a local SQLite database and synchronized directly between peers.
- **CRDT Synchronization:** Leverages [Automerge](https://automerge.org/) to ensure conflict-free data convergence across multiple devices and intermittent connectivity.
- **P2P Networking:** Built on the [Iroh](https://iroh.computer/) stack, utilizing QUIC for encrypted, low-latency transport with built-in hole-punching and relay support.
- **Actor-Based Concurrency:** Implements a robust Actor model (via Tokio) to manage complex P2P connection states, discovery loops, and UI reactivity without shared-state headaches.
- **Cross-Platform Ready:** Includes [UniFFI](https://github.com/mozilla/uniffi-rs) bindings for seamless integration with Swift (iOS) and other high-level languages.

## 🛡️ Research & The "Tryst" Protocol

This project serves as the engineering foundation for my ongoing Master's thesis research on **Metadata Protection** in decentralized networks.

> [!IMPORTANT]
> **Note on Implementation:** To preserve the scientific novelty of my research pending publication, the core logic for the proprietary discovery layer (the "Tryst" protocol) has been omitted from this public showcase.
>
> In this version, peer discovery falls back to the standard **iroh-pkarr-dht** mechanism. This allows the application to remain fully functional while securing the research intellectual property. The architectural stubs remain visible in the source code to demonstrate how the **"Rotating Discovery"** protocol integrates into the larger system.
>
> **Technical Disclaimer:** While the codebase maintains the exact architectural structure of the original research project, certain integration tests and benchmarks that are strictly bound to Tryst-specific cryptographic behaviors or DHT resolution timings may experience failures in this sanitized snapshot.

## 🏗️ Architectural Core

The engine is built on a "local-first" philosophy where the user maintains absolute data ownership. The core components include:

- **Networking (iroh):** High-performance QUIC transport with automatic NAT traversal and authenticated peering.
- **Sync (automerge):** Pure-Rust CRDT implementation ensuring causal consistency and conflict-free merging of message history.
- **Storage (sqlx/sqlite):** Relational persistence for CRDT snapshots and metadata, utilizing an optimized manual compaction strategy to manage history growth.
- **Concurrency (tokio):** An Actor-based model that serializes complex P2P state transitions, ensuring thread-safety and a clean FFI boundary for mobile integration.

## 📂 Project Structure

- `src/app.rs`: Main application entry point and actor orchestration.
- `src/manager.rs`: Coordination of individual chat sessions and CRDT sync.
- `src/persistence.rs`: Bridge between Automerge documents and SQLite.
- `src/protocols/chat.rs`: P2P connection handling and Glare Resolution logic.
- `src/error.rs`: Categorized FFI-friendly error system.

## 🚦 Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) 1.90.0

### Running Tests

The test suite includes full in-memory P2P node simulations.

> [!NOTE]
> These tests rely on the real BitTorrent DHT for discovery. They may take 30-90 seconds to run depending on network conditions.

```bash
echo 'DATABASE_URL="sqlite://tryst.db"' > .env
cargo sqlx database create
cargo sqlx migrate run
cargo test
```

### Benchmarking

Detailed performance metrics for connection establishment and DHT resolution are available in the `bench_results/` directory.

#### Local Execution
Ensure `bench_config.ron` is present in the root directory, then run:
```bash
cargo run --bin bench_orchestrator
```

#### Docker Execution (Recommended)
To ensure a clean, isolated environment with consistent network constraints (latency, packet loss), use the provided Docker wrapper.

**Note:** If you have modified the database schema, run `cargo sqlx prepare` inside `rust/core` before building the Docker image.

```bash
# Run all profiles (baseline, lte, edge)
./scripts/run_docker_benchmarks.sh all

# Or run a specific profile
./scripts/run_docker_benchmarks.sh lte
```
This script handles the setup of isolated P2P nodes in a controlled containerized environment, which is the preferred way to replicate the research results.

## 📜 Roadmap

Detailed future research and engineering goals are tracked in [ROADMAP.md](./ROADMAP.md), including:
- Multi-device identity synchronization.
- Traffic analysis resistance (padding/timing obfuscation).
- Advanced connection verification via iroh EndpointHooks.

## 📄 License

Copyright (c) 2026 Matvii Suk. All rights reserved.

This source code is provided for portfolio evaluation and academic review purposes only. Reproduction, redistribution, or commercial usage is strictly prohibited. See the [LICENSE](../../LICENSE) file in the root directory for full terms.
