//! REPL state that survives across lines.

use std::collections::VecDeque;
use std::net::SocketAddr;

use brain_core::MemoryId;

use crate::parser::OutputFormatArg;

/// Default cap on remembered memory ids for `$N` aliases + completion.
pub const RECENT_ID_CAP: usize = 100;

/// Stateful REPL context.
#[derive(Debug, Clone)]
pub struct Session {
    /// Active transaction id (set by `txn begin`, cleared by
    /// `txn commit` / `txn abort` / `\unset txn`).
    pub active_txn: Option<[u8; 16]>,
    /// Sticky default for `--context` set via `\set context N`.
    pub sticky_context: Option<u64>,
    /// Recently-returned memory ids (newest first, oldest evicted at
    /// [`RECENT_ID_CAP`]).
    pub recent_ids: VecDeque<MemoryId>,
    /// Output renderer choice.
    pub output: OutputFormatArg,
    /// Show per-op elapsed wall time.
    pub timing: bool,
    /// Server endpoint (may change via `\connect`).
    pub server: SocketAddr,
}

impl Session {
    /// Build a fresh session pointing at `server`.
    #[must_use]
    pub fn new(server: SocketAddr, output: OutputFormatArg) -> Self {
        Self {
            active_txn: None,
            sticky_context: None,
            recent_ids: VecDeque::with_capacity(RECENT_ID_CAP),
            output,
            timing: false,
            server,
        }
    }

    /// Build a session seeded from persisted `Settings`. The
    /// caller has already merged `--output` overrides into
    /// `output`; we apply `sticky_context` and `timing` here so the
    /// REPL inherits the user's saved preferences. The `server`
    /// field on `settings` is consumed earlier (when picking the
    /// connect address) and is not duplicated into the session.
    #[must_use]
    pub fn from_settings(
        server: SocketAddr,
        output: OutputFormatArg,
        settings: &crate::cli::config::Settings,
    ) -> Self {
        Self {
            active_txn: None,
            sticky_context: settings.sticky_context,
            recent_ids: VecDeque::with_capacity(RECENT_ID_CAP),
            output,
            timing: settings.timing.unwrap_or(false),
            server,
        }
    }

    /// Push a memory id onto the recent-ids list. Deduplicates: if
    /// the id is already present it's promoted to the front. Evicts
    /// the oldest entry once the cap is exceeded.
    pub fn push_recent_id(&mut self, id: MemoryId) {
        if let Some(pos) = self.recent_ids.iter().position(|x| *x == id) {
            self.recent_ids.remove(pos);
        }
        self.recent_ids.push_front(id);
        while self.recent_ids.len() > RECENT_ID_CAP {
            self.recent_ids.pop_back();
        }
    }

    /// Render the REPL prompt for the current state.
    #[must_use]
    pub fn prompt(&self) -> String {
        let txn_marker = if self.active_txn.is_some() { "*" } else { "" };
        match self.sticky_context {
            Some(ctx) => format!("brain{txn_marker}[ctx={ctx}]> "),
            None => format!("brain{txn_marker}> "),
        }
    }

    /// Helper used by op commands: if the caller didn't supply a
    /// `--txn`, inherit the active session one.
    #[must_use]
    pub fn effective_txn(&self, explicit: Option<[u8; 16]>) -> Option<[u8; 16]> {
        explicit.or(self.active_txn)
    }

    /// Helper used by `encode`: if the caller didn't supply a
    /// `--context`, inherit the sticky one (else 0).
    #[must_use]
    pub fn effective_context(&self, explicit: Option<u64>) -> u64 {
        explicit.or(self.sticky_context).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:9090".parse().expect("parse")
    }

    #[test]
    fn prompt_no_state() {
        let s = Session::new(addr(), OutputFormatArg::Table);
        assert_eq!(s.prompt(), "brain> ");
    }

    #[test]
    fn prompt_with_txn() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.active_txn = Some([0u8; 16]);
        assert_eq!(s.prompt(), "brain*> ");
    }

    #[test]
    fn prompt_with_sticky_context() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.sticky_context = Some(7);
        assert_eq!(s.prompt(), "brain[ctx=7]> ");
    }

    #[test]
    fn prompt_with_both() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.active_txn = Some([0u8; 16]);
        s.sticky_context = Some(7);
        assert_eq!(s.prompt(), "brain*[ctx=7]> ");
    }

    #[test]
    fn recent_id_cap_evicts() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        for i in 0..(RECENT_ID_CAP + 1) {
            s.push_recent_id(MemoryId::from_raw(i as u128));
        }
        assert_eq!(s.recent_ids.len(), RECENT_ID_CAP);
        // The oldest (id = 0) was evicted.
        assert!(!s.recent_ids.iter().any(|x| *x == MemoryId::from_raw(0)));
        // The newest is at the front.
        assert_eq!(
            *s.recent_ids.front().expect("non-empty"),
            MemoryId::from_raw(RECENT_ID_CAP as u128)
        );
    }

    #[test]
    fn recent_id_dedups_to_front() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.push_recent_id(MemoryId::from_raw(1));
        s.push_recent_id(MemoryId::from_raw(2));
        s.push_recent_id(MemoryId::from_raw(1));
        assert_eq!(s.recent_ids.len(), 2);
        assert_eq!(
            *s.recent_ids.front().expect("non-empty"),
            MemoryId::from_raw(1)
        );
    }

    #[test]
    fn effective_txn_inherits_active() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.active_txn = Some([7u8; 16]);
        assert_eq!(s.effective_txn(None), Some([7u8; 16]));
        assert_eq!(s.effective_txn(Some([1u8; 16])), Some([1u8; 16]));
    }

    #[test]
    fn effective_context_inherits_sticky() {
        let mut s = Session::new(addr(), OutputFormatArg::Table);
        s.sticky_context = Some(5);
        assert_eq!(s.effective_context(None), 5);
        assert_eq!(s.effective_context(Some(9)), 9);
        s.sticky_context = None;
        assert_eq!(s.effective_context(None), 0);
    }
}
