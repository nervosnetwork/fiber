pub struct Store {}
pub struct Batch {}
pub enum IteratorMode<'a> {
    Start,
    End,
    From(&'a [u8], DbDirection),
}

pub enum DbDirection {
    Forward,
    Reverse,
}
