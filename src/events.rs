use std::{
    collections::{BTreeMap, LinkedList},
    time::{Duration, Instant},
};

use crate::NetworkServiceEvent;

use log::error;
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use thiserror::Error;

type EventProcessorID = usize;

pub enum EventActorMessage {
    AddOneShotProcessor(Box<dyn EventProcessor>, Duration),
    AddProcessor(EventProcessorID, Box<dyn EventProcessor>),
    RemoveProcessor(EventProcessorID),
    ProcessEvent(Event),
}

#[derive(Debug)]
pub enum Event {
    NetworkServiceEvent(NetworkServiceEvent),
}

#[derive(Error, Debug)]
pub enum ProcessingEventError {}

/// Represents the result of a single EventProcessor's processing of an event.
/// We need to return both the result of the processing and whether we should continue processing.
pub struct ProcessingEventResult {
    /// Determine if we continue processing event with other processors.
    pub should_continue: bool,
    /// Report the result of this event processor.
    pub result: Result<(), ProcessingEventError>,
}

pub trait EventProcessor: Send {
    /// Process any event emitted by this program.
    fn process_event(&self, _event: &Event) -> ProcessingEventResult;
}

#[derive(Default)]
pub struct EventActorState {
    oneshot_processors: LinkedList<(Box<dyn EventProcessor>, Instant)>,
    processors: BTreeMap<EventProcessorID, Box<dyn EventProcessor>>,
}

pub struct EventActor {}

#[rasync_trait]
impl Actor for EventActor {
    type Msg = EventActorMessage;
    type State = EventActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Default::default())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            EventActorMessage::AddOneShotProcessor(processor, duration) => {
                let expiry_time = std::time::Instant::now() + duration;
                state.oneshot_processors.push_back((processor, expiry_time));
            }

            EventActorMessage::AddProcessor(id, processor) => {
                if state.processors.contains_key(&id) {
                    error!("Processor already exists");
                    return Ok(());
                }
                state.processors.insert(id, processor);
            }

            EventActorMessage::RemoveProcessor(id) => {
                state.processors.remove(&id);
            }

            EventActorMessage::ProcessEvent(event) => {
                let mut heads = LinkedList::new();

                loop {
                    let front = state.oneshot_processors.pop_front();
                    match front {
                        Some((processor, expiry_time)) => {
                            if std::time::Instant::now() < expiry_time {
                                let result = processor.process_event(&event);
                                // This processor processed the event, so we remove it from the list,
                                // and break out.
                                if !result.should_continue {
                                    break;
                                }
                                if let Some(err) = result.result.err() {
                                    error!("Error processing event: {:?}", err);
                                }
                                // The event is not processed by this processor, so we add it to the heads list.
                                heads.push_back((processor, expiry_time));
                            }
                        }
                        None => {
                            break;
                        }
                    }
                }
                heads.append(&mut state.oneshot_processors);
                state.oneshot_processors = heads;
            }
        }
        Ok(())
    }
}
