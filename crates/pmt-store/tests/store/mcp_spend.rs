//! What a conversation spent on costed MCP tools (spec: plugin-system, MCP
//! tool costs and spend limits): the total follows the conversation across
//! compression continuations and branches, per server.

use pmt_store::{LineageKind, NewMessage, NewSession, Role, Source};

use crate::support::{TestHome, session};

#[test]
fn the_total_follows_the_conversation_across_compression_and_branches() {
    let home = TestHome::new();
    let store = home.store("default");
    let root = session(&store).id;
    store.add_mcp_spend(root, "renders", 60.0).unwrap();
    let mut continued = NewSession::new(Source::Cli);
    continued.parent = Some((root, LineageKind::Compression));
    let tip = store.create_session(&continued).unwrap().id;
    store.add_mcp_spend(tip, "renders", 30.0).unwrap();
    store
        .append_messages(tip, &[NewMessage::new(Role::User, "draw a cat")])
        .unwrap();
    assert!((store.mcp_spent(root, "renders").unwrap() - 90.0).abs() < 1e-9);
    let branch = store.fork_session(tip, None, None).unwrap().id;
    assert!((store.mcp_spent(branch, "renders").unwrap() - 90.0).abs() < 1e-9);
    assert!(store.mcp_spent(branch, "other").unwrap().abs() < 1e-9);
    let unrelated = session(&store).id;
    assert!(store.mcp_spent(unrelated, "renders").unwrap().abs() < 1e-9);
}
