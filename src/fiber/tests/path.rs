use secp256k1::{PublicKey, Secp256k1, SecretKey};

use crate::fiber::path::{NodeHeap, NodeHeapElement};

#[test]
fn test_node_heap() {
    let secp = Secp256k1::new();
    let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

    let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

    let mut heap = NodeHeap::new(10);
    let node1 = NodeHeapElement {
        node_id: public_key1.into(),
        weight: 0,
        distance: 0,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    let node2 = NodeHeapElement {
        node_id: public_key2.into(),
        weight: 0,
        distance: 0,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    assert!(heap.is_empty());
    heap.push(node1.clone());
    heap.push(node2.clone());
    assert!(!heap.is_empty());
    assert_eq!(heap.pop(), Some(node1));
    assert_eq!(heap.pop(), Some(node2));
    assert_eq!(heap.pop(), None);
    assert!(heap.is_empty());
}

#[test]
fn test_node_heap_probability() {
    let secp = Secp256k1::new();
    let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

    let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

    let mut heap = NodeHeap::new(10);
    let node1 = NodeHeapElement {
        node_id: public_key1.into(),
        weight: 0,
        distance: 0,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    let node2 = NodeHeapElement {
        node_id: public_key2.into(),
        weight: 0,
        distance: 0,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.5,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    heap.push(node1.clone());
    heap.push(node2.clone());
    assert_eq!(heap.pop(), Some(node2));
    assert_eq!(heap.pop(), Some(node1));
    assert_eq!(heap.pop(), None);
}

#[test]
fn test_node_heap_distance() {
    let secp = Secp256k1::new();
    let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

    let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

    let mut heap = NodeHeap::new(10);
    let node1 = NodeHeapElement {
        node_id: public_key1.into(),
        weight: 0,
        distance: 10,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    let node2 = NodeHeapElement {
        node_id: public_key2.into(),
        weight: 0,
        distance: 2,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    heap.push(node1.clone());
    heap.push(node2.clone());
    assert_eq!(heap.pop(), Some(node2));
    assert_eq!(heap.pop(), Some(node1));
    assert_eq!(heap.pop(), None);
}

#[test]
fn test_node_heap_push_or_fix() {
    let secp = Secp256k1::new();
    let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

    let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

    let mut heap = NodeHeap::new(10);
    let node1 = NodeHeapElement {
        node_id: public_key1.into(),
        weight: 0,
        distance: 10,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };
    let node2 = NodeHeapElement {
        node_id: public_key2.into(),
        weight: 0,
        distance: 2,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };

    heap.push(node1.clone());
    heap.push(node2.clone());
    assert_eq!(heap.peek(), Some(node2.clone()).as_ref());

    let node1_update = NodeHeapElement {
        node_id: public_key1.into(),
        weight: 0,
        distance: 1,
        amount_received: 0,
        fee_charged: 0,
        probability: 0.0,
        next_hop: None,
        incoming_tlc_expiry: 0,
    };

    heap.push_or_fix(node1_update.clone());

    assert_eq!(heap.pop(), Some(node1_update));
    assert_eq!(heap.pop(), Some(node2));
    assert_eq!(heap.pop(), None);
}
