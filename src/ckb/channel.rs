#[derive(Debug)]
pub enum Channel {
    Funded(FundedChannel),
    Unfunded(UnfundedChannel),
}

#[derive(Debug)]
pub struct UnfundedChannel {}
#[derive(Debug)]
pub struct FundedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {}

impl Channel {
    pub fn new() -> Self {
        Channel::Unfunded(UnfundedChannel {})
    }

    pub fn step(&mut self, _event: ChannelEvent) {}
}
