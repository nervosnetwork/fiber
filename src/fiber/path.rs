use ckb_types::packed::OutPoint;

use super::types::Pubkey;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Clone, Debug)]
pub(crate) struct NodeHeapElement {
    // node_id is the vertex itself.
    // This can be used to explore all the outgoing edges (channels) emanating from a node.
    pub node_id: Pubkey,

    // The cost from this node to destination node.
    pub weight: u128,

    // The distance from source node to this node.
    pub distance: u128,

    // The amount received by this node.
    pub amount_received: u128,

    // The fee charged by this node.
    pub fee_charged: u128,

    // The probability of this node.
    pub probability: f64,

    // The expected aboslute expiry height for the incoming HTLC of this Node
    pub incoming_cltv_height: u64,

    // next_hop is the edge this route comes from
    pub next_hop: Option<(Pubkey, OutPoint)>,
}

impl Ord for NodeHeapElement {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.distance == other.distance {
            // Use partial_cmp and unwrap_or to handle NaN values
            self.probability
                .partial_cmp(&other.probability)
                .unwrap_or(Ordering::Equal)
        } else {
            other.distance.cmp(&self.distance)
        }
    }
}

impl PartialOrd for NodeHeapElement {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Implement PartialEq for custom comparison
impl PartialEq for NodeHeapElement {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.weight == other.weight
            && self.distance == other.distance
            && self.amount_received == other.amount_received
            && self.fee_charged == other.fee_charged
            && self.probability == other.probability
            && self.next_hop == other.next_hop
    }
}

// Implement Eq for custom comparison
impl Eq for NodeHeapElement {}

pub(crate) struct NodeHeap {
    inner: BinaryHeap<NodeHeapElement>,
}

impl NodeHeap {
    pub fn new(num: usize) -> Self {
        Self {
            inner: BinaryHeap::with_capacity(num),
        }
    }

    pub fn push(&mut self, element: NodeHeapElement) {
        self.inner.push(element);
    }

    pub fn pop(&mut self) -> Option<NodeHeapElement> {
        self.inner.pop()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[allow(dead_code)]
    pub fn peek(&self) -> Option<&NodeHeapElement> {
        self.inner.peek()
    }

    pub fn push_or_fix(&mut self, element: NodeHeapElement) {
        // Remove the element with the same node_id if it exists
        self.inner.retain(|e| e.node_id != element.node_id);
        // Push the new element
        self.inner.push(element);
    }
}

pub(crate) struct ProbabilityEvaluator {}

impl ProbabilityEvaluator {
    pub fn evaluate_probability(
        _from: Pubkey,
        _to: Pubkey,
        _amount_sent: u128,
        _capacity: u128,
    ) -> f64 {
        //FIXME(yukang): to because
        return 0.5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};

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
            incoming_cltv_height: 0,
        };
        let node2 = NodeHeapElement {
            node_id: public_key2.into(),
            weight: 0,
            distance: 0,
            amount_received: 0,
            fee_charged: 0,
            probability: 0.0,
            next_hop: None,
            incoming_cltv_height: 0,
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
            incoming_cltv_height: 0,
        };
        let node2 = NodeHeapElement {
            node_id: public_key2.into(),
            weight: 0,
            distance: 0,
            amount_received: 0,
            fee_charged: 0,
            probability: 0.5,
            next_hop: None,
            incoming_cltv_height: 0,
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
            incoming_cltv_height: 0,
        };
        let node2 = NodeHeapElement {
            node_id: public_key2.into(),
            weight: 0,
            distance: 2,
            amount_received: 0,
            fee_charged: 0,
            probability: 0.0,
            next_hop: None,
            incoming_cltv_height: 0,
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
            incoming_cltv_height: 0,
        };
        let node2 = NodeHeapElement {
            node_id: public_key2.into(),
            weight: 0,
            distance: 2,
            amount_received: 0,
            fee_charged: 0,
            probability: 0.0,
            next_hop: None,
            incoming_cltv_height: 0,
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
            incoming_cltv_height: 0,
        };

        heap.push_or_fix(node1_update.clone());

        assert_eq!(heap.pop(), Some(node1_update));
        assert_eq!(heap.pop(), Some(node2));
        assert_eq!(heap.pop(), None);
    }
}
