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

    // The expected aboslute expiry timestamp (in milliseconds) for the incoming HTLC of this Node
    pub incoming_tlc_expiry: u64,

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
            && self.incoming_tlc_expiry == other.incoming_tlc_expiry
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
