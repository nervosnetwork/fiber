//! Per-node chain fault injection for simulated CKB actor, client, and watchtower calls.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fnn::ckb::tests::test_utils::{MockChainActorMiddleware, MockChainActorState};
use fnn::ckb::CkbChainMessage;
use fnn::simulation::SimulatedChainClientHook;
use ractor::{ActorProcessingErr, ActorRef};
use tokio::sync::watch;

/// Faults affecting one node's access to the simulated chain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChainFault {
    /// The chain is reachable.
    #[default]
    Healthy,
    /// A request takes this much additional virtual time.
    Slow(Duration),
    /// A request returns no response after this much virtual time.
    Timeout(Duration),
    /// Requests remain pending until the test restores access.
    Offline,
}

/// A caller of the shared chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainEndpoint {
    /// Funding, signing, and transaction tracing commands.
    Actor,
    /// Direct CKB RPC-style queries.
    Client,
    /// The watchtower reads or submits a transaction.
    Watchtower,
}

/// A request that encountered a simulated chain fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainFaultHit {
    /// Which chain integration path made the request.
    pub endpoint: ChainEndpoint,
    /// Name of the command or query.
    pub request: &'static str,
    /// The fault observed by the request.
    pub fault: ChainFault,
}

/// Per-node fault control for a chain shared by multiple nodes.
#[derive(Clone, Debug)]
pub struct SimulatedChainAccess {
    fault: watch::Sender<ChainRule>,
    hits: Arc<Mutex<Vec<ChainFaultHit>>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ChainRule {
    fault: ChainFault,
    target: Option<(ChainEndpoint, &'static str)>,
}

impl ChainRule {
    fn for_request(self, endpoint: ChainEndpoint, request: &str) -> ChainFault {
        if self
            .target
            .is_none_or(|target| target == (endpoint, request))
        {
            self.fault
        } else {
            ChainFault::Healthy
        }
    }
}

impl Default for SimulatedChainAccess {
    fn default() -> Self {
        Self {
            fault: watch::Sender::new(ChainRule::default()),
            hits: Arc::default(),
        }
    }
}

impl SimulatedChainAccess {
    /// Change the fault for subsequent and blocked chain requests.
    pub fn set(&self, fault: ChainFault) {
        self.fault.send_replace(ChainRule {
            fault,
            target: None,
        });
    }

    /// Target one named request while other chain operations proceed normally.
    pub fn set_for(&self, endpoint: ChainEndpoint, request: &'static str, fault: ChainFault) {
        self.fault.send_replace(ChainRule {
            fault,
            target: Some((endpoint, request)),
        });
    }

    /// Drain observed faults for the deterministic trace.
    pub fn take_hits(&self) -> Vec<ChainFaultHit> {
        std::mem::take(&mut *self.hits.lock().expect("chain fault hits"))
    }

    pub(crate) async fn before(
        &self,
        endpoint: ChainEndpoint,
        request: &'static str,
    ) -> anyhow::Result<()> {
        let mut rule = self.fault.subscribe();
        let initial = rule.borrow_and_update().for_request(endpoint, request);
        if initial == ChainFault::Offline {
            self.record(endpoint, request, initial);
        }
        let active = if initial == ChainFault::Offline {
            rule.wait_for(|rule| rule.for_request(endpoint, request) != ChainFault::Offline)
                .await?
                .for_request(endpoint, request)
        } else {
            initial
        };
        match active {
            ChainFault::Slow(duration) => {
                self.record(endpoint, request, active);
                tokio::time::sleep(duration).await;
            }
            ChainFault::Timeout(duration) => {
                tokio::time::sleep(duration).await;
                self.record(endpoint, request, active);
                anyhow::bail!("simulated CKB {request} timed out");
            }
            _ => {}
        }
        Ok(())
    }

    fn record(&self, endpoint: ChainEndpoint, request: &'static str, fault: ChainFault) {
        self.hits
            .lock()
            .expect("chain fault hits")
            .push(ChainFaultHit {
                endpoint,
                request,
                fault,
            });
    }
}

#[async_trait::async_trait]
impl SimulatedChainClientHook for SimulatedChainAccess {
    async fn before_client_request(&self, request: &'static str) -> anyhow::Result<()> {
        self.before(ChainEndpoint::Client, request).await
    }
}

#[async_trait::async_trait]
impl MockChainActorMiddleware for SimulatedChainAccess {
    async fn handle(
        &mut self,
        _: ActorRef<CkbChainMessage>,
        message: CkbChainMessage,
        _: &mut MockChainActorState,
    ) -> Result<Option<CkbChainMessage>, ActorProcessingErr> {
        let request = match &message {
            CkbChainMessage::Fund(..) => "Fund",
            CkbChainMessage::BuildUnsignedFundingTx { .. } => "BuildUnsignedFundingTx",
            CkbChainMessage::VerifyFundingTx { .. } => "VerifyFundingTx",
            CkbChainMessage::Sign(..) => "Sign",
            CkbChainMessage::SendTx(..) => "SendTx",
            CkbChainMessage::CreateTxTracer(..) => "CreateTxTracer",
            _ => return Ok(Some(message)),
        };
        // A timed-out actor command drops its RPC reply port or tracing callback.
        Ok(self
            .before(ChainEndpoint::Actor, request)
            .await
            .is_ok()
            .then_some(message))
    }

    fn clone_box(&self) -> Box<dyn MockChainActorMiddleware> {
        Box::new(self.clone())
    }
}
