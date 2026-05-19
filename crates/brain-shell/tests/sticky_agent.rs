//! `\agent use` semantics: stashing a sticky agent_id on the session.
//! The wire-level reconnect path is exercised by E2E tests; here we
//! check that the in-process state mutation behaves.

use std::net::SocketAddr;

use brain_core::AgentId;
use brain_shell::parser::OutputFormatArg;
use brain_shell::session::Session;
use uuid::Uuid;

fn addr() -> SocketAddr {
    "127.0.0.1:9090".parse().unwrap()
}

#[test]
fn fresh_session_has_no_sticky_agent() {
    let s = Session::new(addr(), OutputFormatArg::Table);
    assert!(s.sticky_agent.is_none());
}

#[test]
fn sticky_agent_persists_across_mutation() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    let uuid = Uuid::now_v7();
    s.sticky_agent = Some(AgentId(uuid));
    // Modify some other fields to ensure they don't clobber sticky_agent.
    s.sticky_context = Some(7);
    s.active_txn = Some([1u8; 16]);
    assert_eq!(s.sticky_agent, Some(AgentId(uuid)));
}

#[test]
fn sticky_agent_clones_through_session_clone() {
    let mut s = Session::new(addr(), OutputFormatArg::Table);
    s.sticky_agent = Some(AgentId(Uuid::now_v7()));
    let cloned = s.clone();
    assert_eq!(cloned.sticky_agent, s.sticky_agent);
}
