mod pcn;

// pcn module requires types from blockchain.
// We need to re-export these types.
mod blockchain {
    pub use ckb_gen_types::packed::*;
}
