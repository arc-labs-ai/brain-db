//! LINK / UNLINK handler scaffold.
//!
//! Sub-task 7.8 lands the real handlers. The wire shape doesn't
//! exist in `brain-protocol` yet (Phase 1 didn't ship `LinkRequest`
//! / `UnlinkRequest`); 7.8 will extend it first and then add the
//! `RequestBody::Link(_)` / `Unlink(_)` arms to the dispatcher.
//!
//! Until 7.8: this module is intentionally empty. Keeping it as a
//! file so the module structure is in place when 7.8 lands.
