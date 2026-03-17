/// Direction for iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorDirection {
    Forward,
    Reverse,
}

/// A key-value pair returned from iteration.
#[derive(Debug, Clone)]
pub struct KVPair {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
