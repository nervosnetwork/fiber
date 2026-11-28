use futures::FutureExt;

use crate::{fiber::channel::ChannelActorState, store::sample::StoreSample};

use super::{
    notifications::{SigningChange, SigningScope},
    SigningEvents,
};

#[test]
fn updates_publish_only_explicitly_and_preserve_channel_migration_order() {
    let events = SigningEvents::default();
    let publisher = events.channels(None);
    let mut changes = events.hub.subscribe(SigningScope::Channel(None));
    let mut state = ChannelActorState::samples(42).remove(0);
    let old_id = state.get_id();

    // Dropping a guard without publishing emits no captured event.
    drop(publisher.prepare(&state));
    assert!(changes.recv().now_or_never().is_none());

    let update = publisher.prepare(&state);
    assert!(changes.recv().now_or_never().is_none());
    update.publish();
    assert_eq!(
        changes.recv().now_or_never().unwrap().unwrap().channel_id(),
        old_id
    );

    let new_id = [8; 32].into();
    state.id = new_id;
    let update = publisher.prepare(&state);
    update.publish_removed(old_id);
    update.publish();
    assert!(
        matches!(changes.recv().now_or_never().unwrap().unwrap().as_ref(), SigningChange::Removed(id) if *id == old_id)
    );
    assert!(
        matches!(changes.recv().now_or_never().unwrap().unwrap().as_ref(), SigningChange::Channel { channel_id, .. } if *channel_id == new_id)
    );
}
