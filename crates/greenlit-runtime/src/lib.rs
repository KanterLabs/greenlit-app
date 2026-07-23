//! Container runtime for Greenlit: the `ContainerEngine` trait, its
//! bollard-backed Docker implementation, base-image construction, and
//! overlayfs workspace isolation.
//!
//! Phase 2 (execution) fills this crate in. All host container access flows
//! through the `ContainerEngine` trait so non-Linux/x86_64 backends remain
//! ports rather than forks.
#![forbid(unsafe_code)]
