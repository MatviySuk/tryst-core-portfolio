# Gossip Project Roadmap

This document outlines planned improvements and future research directions for the Gossip P2P communication system.

## Protocol & Architecture
- [ ] **Open Invitation Support:** Implement iroh Hooks to support accepting and resolving open invitations.
- [ ] **Simultaneous Connection Scaling:** Transition the `ChatProtocolActor` to an Actor-based `ChatConnectionsManager` to support establishing multiple connections simultaneously in separate tasks.
- [ ] **Identity & Device Management:** 
  - Decouple Account IDs from Endpoint IDs to support regular rotation of endpoint identifiers.
  - Implement multi-device synchronization support.
- [ ] **Connection Verification:** Add verification logic to ensure accepted connections correspond to authorized `StoredTryst` records.

## Metadata Protection (Tryst Research)
- [ ] **Conflict Resolution:** Refine logic for handling multiple invitations or discovery records from the same identity across different epochs.
- [ ] **Traffic Analysis Resistance:** Implement padding and timing obfuscation to further protect against Global Passive Adversaries (GPA).

## Reliability & UX
- [ ] **Enhanced Testing:** Complete the integration test suite for Open Invitations.
- [ ] **User-Facing Error Categorization:** Refine the FFI error boundary to provide more granular, actionable feedback to client applications (e.g., distinguishing between temporary network churn and permanent authentication failure).
