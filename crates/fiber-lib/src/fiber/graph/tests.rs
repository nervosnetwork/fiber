use super::*;

impl<S> NetworkGraph<S>
where
    S: NetworkGraphStateStore
        + ChannelActorStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub(crate) fn set_fixed_rand_expiry_delta(&mut self, expiry_delta: u64) {
        self.fixed_rand_expiry_delta = Some(expiry_delta);
    }
}
