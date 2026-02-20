#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::channel::ChannelActorState;
use fnn::fiber::history::TimedResult;
use fnn::fiber::network::PersistentNetworkActorState;
use fnn::fiber::payment::PaymentSession;
use fnn::fiber::types::BroadcastMessage;
use fnn::fiber::PaymentCustomRecords;
use fnn::invoice::{CkbInvoice, CkbInvoiceStatus};

fuzz_target!(|data: &[u8]| {
    // Fuzz bincode deserialization for all store-persisted types.
    // These exercise corrupted data resilience: the store may contain
    // malformed data due to crashes, disk errors, or version mismatches.
    //
    // Any panic (not Err) here indicates a bug in the deserialization logic.

    let _ = bincode::deserialize::<ChannelActorState>(data);
    let _ = bincode::deserialize::<PersistentNetworkActorState>(data);
    let _ = bincode::deserialize::<PaymentSession>(data);
    let _ = bincode::deserialize::<TimedResult>(data);
    let _ = bincode::deserialize::<CkbInvoice>(data);
    let _ = bincode::deserialize::<CkbInvoiceStatus>(data);
    let _ = bincode::deserialize::<BroadcastMessage>(data);
    let _ = bincode::deserialize::<PaymentCustomRecords>(data);
});
