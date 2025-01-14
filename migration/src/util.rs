use serde::{Deserialize, Serialize};

pub(crate) fn convert<Old, New>(old: Old) -> New
where
    Old: Serialize,
    New: for<'de> Deserialize<'de>,
{
    let buf = bincode::serialize(&old).unwrap();
    let new_value: New = bincode::deserialize(&buf).expect("deserialize to new state");
    new_value
}
