use std::cmp::Ordering;
use std::collections::BinaryHeap;

use ractor::{Actor, ActorProcessingErr, ActorRef};
use tokio::task::AbortHandle;
use tokio::time::Duration;

use crate::cch::{
    order::{CchOrderStatus, CchOrderStore},
    trackers::LndTrackerMessage,
    CchError,
};
use crate::fiber::types::Hash256;
use crate::time::{SystemTime, UNIX_EPOCH};

const PRUNE_DELAY_DAYS: u64 = 21;
pub const PRUNE_DELAY_SECONDS: u64 = PRUNE_DELAY_DAYS * 24 * 60 * 60;

/// Get the current time in seconds since UNIX epoch
fn current_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time should always be after UNIX_EPOCH")
        .as_secs()
}

/// Scheduled job types for order expiry and pruning
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduledJob {
    /// Expire an order at the specified time
    ExpireOrder {
        payment_hash: Hash256,
        expiry_time: u64,
    },
    /// Prune a final order at the specified time
    PruneOrder {
        payment_hash: Hash256,
        prune_time: u64,
    },
}

impl ScheduledJob {
    /// Get the execution time for this job
    fn execution_time(&self) -> u64 {
        match self {
            ScheduledJob::ExpireOrder { expiry_time, .. } => *expiry_time,
            ScheduledJob::PruneOrder { prune_time, .. } => *prune_time,
        }
    }
}

impl Ord for ScheduledJob {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order by execution time (earliest first)
        // Use reverse ordering because BinaryHeap is a max-heap
        other.execution_time().cmp(&self.execution_time())
    }
}

impl PartialOrd for ScheduledJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Messages for the scheduler actor
#[derive(Debug)]
pub enum SchedulerMessage {
    /// Schedule an expiry job for an order (scheduler will calculate expiry time)
    ScheduleExpiry {
        payment_hash: Hash256,
        created_at: u64,
        expiry_delta_seconds: u64,
    },
    /// Schedule a prune job for a final order (scheduler will calculate prune time)
    SchedulePrune {
        payment_hash: Hash256,
        created_at: u64,
        expiry_delta_seconds: u64,
    },
    /// Process due jobs (internal message)
    ProcessJobs,
}

/// Arguments for starting the scheduler actor
pub struct SchedulerArgs<S> {
    pub store: S,
    pub lnd_tracker: ActorRef<LndTrackerMessage>,
}

/// State for the scheduler actor
pub struct SchedulerState<S> {
    store: S,
    lnd_tracker: ActorRef<LndTrackerMessage>,
    jobs: BinaryHeap<ScheduledJob>,
    processing_abort_handle: Option<AbortHandle>,
}

pub struct CchOrderSchedulerActor<S>(std::marker::PhantomData<S>);

impl<S> Default for CchOrderSchedulerActor<S> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<S: CchOrderStore + Send + Sync + Clone + 'static> CchOrderSchedulerActor<S> {
    pub async fn start(
        args: SchedulerArgs<S>,
        root_actor: ractor::ActorCell,
    ) -> Result<ActorRef<SchedulerMessage>, ractor::ActorProcessingErr> {
        let (actor, _handle) =
            ractor::Actor::spawn_linked(None, CchOrderSchedulerActor::default(), args, root_actor)
                .await?;
        Ok(actor)
    }
}

#[async_trait::async_trait]
impl<S: CchOrderStore + Send + Sync + 'static> Actor for CchOrderSchedulerActor<S> {
    type Msg = SchedulerMessage;
    type State = SchedulerState<S>;
    type Arguments = SchedulerArgs<S>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let state = SchedulerState {
            store: args.store,
            lnd_tracker: args.lnd_tracker,
            jobs: BinaryHeap::new(),
            processing_abort_handle: None,
        };

        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SchedulerMessage::ScheduleExpiry {
                payment_hash,
                created_at,
                expiry_delta_seconds,
            } => {
                let expiry_time = created_at.saturating_add(expiry_delta_seconds);
                state.push_job(
                    ScheduledJob::ExpireOrder {
                        payment_hash,
                        expiry_time,
                    },
                    myself,
                    current_time_secs(),
                );
                Ok(())
            }
            SchedulerMessage::SchedulePrune {
                payment_hash,
                created_at,
                expiry_delta_seconds,
            } => {
                // Calculate prune time
                let prune_time = created_at
                    .saturating_add(expiry_delta_seconds)
                    .saturating_add(PRUNE_DELAY_SECONDS);
                state.push_job(
                    ScheduledJob::PruneOrder {
                        payment_hash,
                        prune_time,
                    },
                    myself,
                    current_time_secs(),
                );
                Ok(())
            }
            SchedulerMessage::ProcessJobs => {
                let current_time = current_time_secs();
                state.process_due_jobs(myself, current_time);
                Ok(())
            }
        }
    }
}

impl<S: CchOrderStore> SchedulerState<S> {
    fn push_job(
        &mut self,
        job: ScheduledJob,
        mailbox: ActorRef<SchedulerMessage>,
        current_time: u64,
    ) {
        tracing::debug!("Scheduled job: {:?}", job);
        self.jobs.push(job);
        self.schedule_next_process(mailbox, current_time);
    }

    /// Extract all jobs that are due (execution_time <= current_time) from the heap
    fn extract_due_jobs(&mut self, current_time: u64) -> Vec<ScheduledJob> {
        let mut due_jobs = Vec::new();
        while let Some(job) = self.jobs.peek() {
            if job.execution_time() <= current_time {
                due_jobs.push(self.jobs.pop().unwrap());
            } else {
                break;
            }
        }
        due_jobs
    }

    /// Process all due jobs and schedule the next ProcessJobs if there are remaining jobs
    fn process_due_jobs(&mut self, mailbox: ActorRef<SchedulerMessage>, current_time: u64) {
        // Process all due jobs
        let due_jobs = self.extract_due_jobs(current_time);

        for job in due_jobs {
            match job {
                ScheduledJob::ExpireOrder { payment_hash, .. } => {
                    if let Err(err) = self.expire_order(mailbox.clone(), payment_hash) {
                        tracing::error!("Failed to expire order {:x}: {}", payment_hash, err);
                    }
                }
                ScheduledJob::PruneOrder { payment_hash, .. } => {
                    if let Err(err) = self.prune_order(payment_hash) {
                        tracing::error!("Failed to prune order {:x}: {}", payment_hash, err);
                    }
                }
            }
        }

        // Schedule next ProcessJobs if there are remaining jobs
        self.schedule_next_process(mailbox, current_time);
    }

    /// Schedule the next ProcessJobs message based on the earliest job in the heap
    fn schedule_next_process(&mut self, mailbox: ActorRef<SchedulerMessage>, current_time: u64) {
        // Abort any existing scheduled process
        if let Some(handle) = self.processing_abort_handle.take() {
            handle.abort();
        }

        if self.jobs.is_empty() {
            return;
        }

        let next_job_time = self.jobs.peek().unwrap().execution_time();

        // If the job is already due, process immediately
        if next_job_time <= current_time {
            let _ = mailbox.send_message(SchedulerMessage::ProcessJobs);
            return;
        }

        let delay_secs = next_job_time - current_time;
        let delay = Duration::from_secs(delay_secs.min(3600));
        let handle = mailbox.send_after(delay, move || SchedulerMessage::ProcessJobs);
        self.processing_abort_handle = Some(handle.abort_handle());
    }

    /// Handle expiring an order
    fn expire_order(
        &self,
        mailbox: ActorRef<SchedulerMessage>,
        payment_hash: Hash256,
    ) -> Result<(), CchError> {
        let mut order = match self.store.get_cch_order(&payment_hash) {
            Ok(order) => order,
            Err(_) => return Ok(()),
        };

        // Skip if order is already final
        if order.is_final() {
            tracing::debug!("Order {:x} is already final, skipping expiry", payment_hash);
            return Ok(());
        }

        // Store order info before updating
        let created_at = order.created_at;
        let expiry_delta_seconds = order.expiry_delta_seconds;

        order.status = CchOrderStatus::Failed;
        order.failure_reason = Some("Order expired".to_string());
        self.store.update_cch_order(order);
        tracing::info!("Expired order {:x}", payment_hash);

        // Schedule prune job for this expired order
        if let Err(err) = mailbox.send_message(SchedulerMessage::SchedulePrune {
            payment_hash,
            created_at,
            expiry_delta_seconds,
        }) {
            tracing::error!(
                "Failed to schedule prune job for expired order {:x}: {}",
                payment_hash,
                err
            );
        }

        Ok(())
    }

    /// Handle pruning a final order
    fn prune_order(&self, payment_hash: Hash256) -> Result<(), CchError> {
        let order = match self.store.get_cch_order(&payment_hash) {
            Ok(order) => order,
            Err(_) => return Ok(()),
        };

        if !order.is_final() {
            tracing::debug!("Order {:x} is not final, skipping prune", payment_hash);
            return Ok(());
        }

        // Ignore the error because LndTrackerActor may exit earlier.
        let _ = self
            .lnd_tracker
            .send_message(LndTrackerMessage::StopTracking(payment_hash));

        // Delete order from store
        self.store.delete_cch_order(&payment_hash);
        tracing::info!("Pruned final order {:x}", payment_hash);

        Ok(())
    }
}
