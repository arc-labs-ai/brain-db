//! Crossterm event loop. The follow-up plan implements blocking +
//! polling event ingestion, terminal resize handling, and a tick
//! channel for periodic refresh. The stub exists so the module path
//! is reserved.

#![allow(dead_code)]

/// Marker for the eventual event-loop driver. Replaced by a real
/// channel-backed type in the follow-up plan.
#[derive(Debug, Default)]
pub struct EventLoop;
