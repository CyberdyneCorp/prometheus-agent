//! Pinned messages (spec: context-compression, Pinned messages survive
//! compaction): the text outlives its row and leaves with the session.

use pmt_core::id::MessageId;

use crate::support::{TestHome, session};

#[test]
fn pins_keep_their_text_in_pin_order_and_leave_with_the_session() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    let (first, second) = (MessageId::new(), MessageId::new());
    store
        .pin(s, second, "never touch the prod database")
        .unwrap();
    store.pin(s, first, "use the staging bucket").unwrap();
    store
        .pin(s, second, "never touch the prod database")
        .unwrap();
    let texts: Vec<String> = store.pins(s).unwrap().into_iter().map(|p| p.text).collect();
    assert_eq!(
        texts,
        ["never touch the prod database", "use the staging bucket"]
    );

    assert!(store.unpin(s, first).unwrap());
    assert!(!store.unpin(s, first).unwrap());
    assert_eq!(store.pins(s).unwrap().len(), 1);

    store.delete_session(s, false).unwrap();
    assert!(store.pins(s).unwrap().is_empty());
}
