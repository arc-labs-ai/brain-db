//! Session-state tests.

use std::net::SocketAddr;

use brain_core::MemoryId;
use brain_shell::parser::OutputFormatArg;
use brain_shell::session::{Session, RECENT_ID_CAP};

fn addr() -> SocketAddr {
    "127.0.0.1:9090".parse().expect("addr")
}

#[test]
fn prompt_default_is_brain_arrow() {
    let s = Session::new(addr(), OutputFormatArg::Table);
    assert_eq!(s.prompt(), "brain> ");
}

#[test]
fn prompt_marks_active_txn() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.active_txn = Some([0xAB; 16]);
    assert_eq!(s.prompt(), "brain*> ");
}

#[test]
fn prompt_shows_sticky_context() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.sticky_context = Some(42);
    assert_eq!(s.prompt(), "brain[ctx=42]> ");
}

#[test]
fn prompt_combines_txn_and_context() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.active_txn = Some([1; 16]);
    s.sticky_context = Some(3);
    assert_eq!(s.prompt(), "brain*[ctx=3]> ");
}

#[test]
fn recent_id_evicts_oldest_past_cap() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    for i in 0..=(RECENT_ID_CAP as u128) {
        s.push_recent_id(MemoryId::from_raw(i));
    }
    let snap = s.recent_ids_snapshot();
    assert_eq!(snap.len(), RECENT_ID_CAP);
    assert!(!snap.iter().any(|x| *x == MemoryId::from_raw(0)));
    assert_eq!(snap[0], MemoryId::from_raw(RECENT_ID_CAP as u128));
}

#[test]
fn recent_id_dedup_promotes_to_front() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.push_recent_id(MemoryId::from_raw(1));
    s.push_recent_id(MemoryId::from_raw(2));
    s.push_recent_id(MemoryId::from_raw(3));
    s.push_recent_id(MemoryId::from_raw(2));
    let ids: Vec<u128> = s
        .recent_ids_snapshot()
        .into_iter()
        .map(|m| m.raw())
        .collect();
    assert_eq!(ids, vec![2, 3, 1]);
}

#[test]
fn effective_txn_inherits_active() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.active_txn = Some([9; 16]);
    assert_eq!(s.effective_txn(None), Some([9; 16]));
    assert_eq!(s.effective_txn(Some([1; 16])), Some([1; 16]));
}

#[test]
fn effective_context_inherits_sticky() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.sticky_context = Some(11);
    assert_eq!(s.effective_context(None), 11);
    assert_eq!(s.effective_context(Some(5)), 5);
}

// ---------------------------------------------------------------------------
// is_txn_terminal — the helper the REPL loop uses to decide whether a
// failed op means the session's tracked txn is gone (Part C of the
// txn-expired-cascade fix).
// ---------------------------------------------------------------------------

#[test]
fn is_txn_terminal_matches_txn_not_found_and_transaction_timeout() {
    use brain_protocol::ErrorCodeWire;
    use brain_sdk_rust::ClientError;
    use brain_shell::commands::is_txn_terminal;

    let not_found = ClientError::Server {
        code: ErrorCodeWire::TxnNotFound as u16,
        message: "transaction not found".into(),
    };
    assert!(is_txn_terminal(&not_found));

    let timeout = ClientError::Server {
        code: ErrorCodeWire::TransactionTimeout as u16,
        message: "transaction expired".into(),
    };
    assert!(is_txn_terminal(&timeout));
}

#[test]
fn is_txn_terminal_ignores_other_codes() {
    use brain_protocol::ErrorCodeWire;
    use brain_sdk_rust::ClientError;
    use brain_shell::commands::is_txn_terminal;

    for code in [
        ErrorCodeWire::IdempotencyConflict,
        ErrorCodeWire::InvalidArgument,
        ErrorCodeWire::Overloaded,
        ErrorCodeWire::MemoryNotFound,
    ] {
        let e = ClientError::Server {
            code: code as u16,
            message: format!("{code:?}"),
        };
        assert!(
            !is_txn_terminal(&e),
            "{code:?} must NOT be classified as terminal txn"
        );
    }
}

#[test]
fn is_txn_terminal_ignores_non_server_errors() {
    use brain_sdk_rust::ClientError;
    use brain_shell::commands::is_txn_terminal;

    let internal = ClientError::Internal("boom".into());
    assert!(!is_txn_terminal(&internal));

    let pool_closed = ClientError::PoolClosed;
    assert!(!is_txn_terminal(&pool_closed));
}
