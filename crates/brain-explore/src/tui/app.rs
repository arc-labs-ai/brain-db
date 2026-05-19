//! The top-level `App` value: owns the event loop driver, the current
//! `AppState`, and the panel set. The follow-up plan turns this into
//! the immediate-mode update/draw loop that crossterm + ratatui drive;
//! today it is a stub so consumer code that needs to refer to `App` by
//! name compiles.

#![allow(dead_code)]

/// The interactive explorer's root value. The follow-up plan replaces
/// the unit body with a real state machine (current focus, modal
/// stack, per-panel substate, refresh ticker).
#[derive(Debug, Default)]
pub struct App;
