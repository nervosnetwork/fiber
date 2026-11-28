use super::*;

struct LockDepResolver {
    lock: Script,
    dep: Option<packed::CellDep>,
}

impl CellDepResolver for LockDepResolver {
    fn resolve(&self, script: &Script) -> Option<packed::CellDep> {
        (script == &self.lock).then(|| self.dep.clone()).flatten()
    }
}

fn builder(peer_tx: Option<TransactionView>) -> FundingTxBuilder {
    let script = Script::default();
    FundingTxBuilder {
        funding_tx: FundingTx { tx: peer_tx },
        request: FundingRequest::default(),
        context: FundingContext {
            rpc_url: String::new(),
            funding_source_lock_script: script.clone(),
            funding_source_lock_script_cell_deps: vec![],
            funding_cell_lock_script: script,
            funding_udt_type_script: None,
        },
    }
}

fn output(capacity: u64) -> packed::CellOutput {
    packed::CellOutput::new_builder()
        .capacity(Capacity::shannons(capacity).pack())
        .build()
}

fn input(seed: u8) -> CellInput {
    CellInput::new(
        packed::OutPoint::new(packed::Byte32::from_slice(&[seed; 32]).unwrap(), 0),
        0,
    )
}

#[test]
fn test_funding_peer_output_order_with_local_udt_change() {
    let peer_tx = packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(vec![output(100), output(200), output(300)])
        .set_outputs_data(vec![[1].pack(), [2].pack(), [3].pack()])
        .build();
    let tx = builder(Some(peer_tx)).assemble_base_tx(
        output(400),
        [4].pack(),
        vec![],
        vec![output(500)],
        vec![[5].pack()],
        HashSet::new(),
    );

    assert_eq!(
        tx.outputs().into_iter().collect::<Vec<_>>(),
        vec![output(400), output(200), output(300), output(500)]
    );
    assert_eq!(
        tx.outputs_data().into_iter().collect::<Vec<_>>(),
        vec![[4].pack(), [2].pack(), [3].pack(), [5].pack()]
    );
}

#[test]
fn test_funding_local_udt_witness_after_peer_inputs() {
    let peer_tx = packed::Transaction::default()
        .as_advanced_builder()
        .set_inputs(vec![input(1), input(2)])
        .build();
    let tx = builder(Some(peer_tx)).assemble_base_tx(
        output(400),
        packed::Bytes::default(),
        vec![input(3), input(4)],
        vec![],
        vec![],
        HashSet::new(),
    );
    let witnesses: Vec<_> = tx.witnesses().into_iter().collect();
    assert_eq!(witnesses.len(), 3);
    assert!(is_secp_sighash_placeholder_witness(
        witnesses[0].raw_data().as_ref()
    ));
    assert!(witnesses[1].raw_data().is_empty());
    assert!(is_secp_sighash_placeholder_witness(
        witnesses[2].raw_data().as_ref()
    ));
    let signed_witness = packed::WitnessArgs::new_builder()
        .lock(Some(molecule::bytes::Bytes::from(vec![1u8; 65])).pack())
        .build();
    assert_eq!(
        witnesses[2].raw_data().len(),
        signed_witness.as_slice().len()
    );
    let signed_tx = tx
        .as_advanced_builder()
        .set_witnesses(vec![
            witnesses[0].clone(),
            witnesses[1].clone(),
            signed_witness.as_bytes().pack(),
        ])
        .build();
    assert_eq!(
        tx.data().as_slice().len(),
        signed_tx.data().as_slice().len()
    );
}

#[test]
fn test_funding_local_udt_witness_first_input() {
    let tx = builder(None).assemble_base_tx(
        output(400),
        packed::Bytes::default(),
        vec![input(3), input(4)],
        vec![],
        vec![],
        HashSet::new(),
    );
    assert_eq!(tx.witnesses().len(), 1);
    assert!(is_secp_sighash_placeholder_witness(
        tx.witnesses().get(0).unwrap().raw_data().as_ref()
    ));
}

#[test]
fn test_funding_udt_lock_dep_without_ckb_inputs() {
    let builder = builder(None);
    let dep = packed::CellDep::new_builder().build();
    let resolver = LockDepResolver {
        lock: builder.context.funding_source_lock_script.clone(),
        dep: Some(dep.clone()),
    };
    let mut deps = HashSet::new();
    builder
        .add_udt_lock_dep(&resolver, &[input(1), input(2)], &mut deps)
        .expect("UDT lock dependency resolved");
    let tx = builder.assemble_base_tx(
        output(400),
        packed::Bytes::default(),
        vec![input(1), input(2)],
        vec![],
        vec![],
        deps,
    );
    assert_eq!(tx.cell_deps().len(), 1);
    assert_eq!(tx.cell_deps().get(0), Some(dep));
}

#[test]
fn test_funding_udt_lock_dep_is_deduplicated() {
    let builder = builder(None);
    let dep = packed::CellDep::new_builder().build();
    let resolver = LockDepResolver {
        lock: builder.context.funding_source_lock_script.clone(),
        dep: Some(dep.clone()),
    };
    let mut deps = HashSet::from([dep]);
    builder
        .add_udt_lock_dep(&resolver, &[input(1)], &mut deps)
        .expect("existing dependency is valid");
    assert_eq!(deps.len(), 1);
}

#[test]
fn test_funding_udt_lock_dep_missing_fails() {
    let builder = builder(None);
    let resolver = LockDepResolver {
        lock: builder.context.funding_source_lock_script.clone(),
        dep: None,
    };
    let mut deps = HashSet::new();
    assert!(matches!(
        builder.add_udt_lock_dep(&resolver, &[input(1)], &mut deps),
        Err(TxBuilderError::ResolveCellDepFailed(_))
    ));
    builder
        .add_udt_lock_dep(&resolver, &[], &mut deps)
        .expect("no UDT input needs no lock dep");
}
