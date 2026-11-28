use super::*;

impl LspDeliveryPort {
    pub(crate) fn without_fast_path(actor: ActorRef<LspServiceMessage>) -> Arc<Self> {
        Arc::new(Self {
            actor,
            metadata_store: None,
        })
    }
}
