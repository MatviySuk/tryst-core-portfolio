//! gossip-core: A local-first, metadata-resistant P2P communication library.
//!
//! This crate provides the foundational logic for decentralized messaging,
//! including P2P networking (iroh), CRDT-based synchronization (automerge),
//! and persistent storage (sqlite). The architecture follows an Actor-based
//! model to manage high-concurrency P2P streams and cross-platform UI integration.

pub mod app;
pub mod bench_utils;
pub mod error;
pub mod log;
pub mod manager;
pub mod network_endpoint;
pub mod persistence;
pub mod protocols;

uniffi::setup_scaffolding!();
