use std::error::Error;
use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArithmeticError {
    message: String,
}

impl ArithmeticError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ArithmeticError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ArithmeticError {}

pub(crate) fn checked_add_u64(lhs: u64, rhs: u64, context: &str) -> Result<u64, ArithmeticError> {
    lhs.checked_add(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} overflows: {} + {}", context, lhs, rhs)))
}

pub(crate) fn checked_sub_u64(lhs: u64, rhs: u64, context: &str) -> Result<u64, ArithmeticError> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} underflows: {} - {}", context, lhs, rhs)))
}

#[cfg(feature = "watchtower")]
pub(crate) fn checked_mul_u64(lhs: u64, rhs: u64, context: &str) -> Result<u64, ArithmeticError> {
    lhs.checked_mul(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} overflows: {} * {}", context, lhs, rhs)))
}

pub(crate) fn checked_add_u128(
    lhs: u128,
    rhs: u128,
    context: &str,
) -> Result<u128, ArithmeticError> {
    lhs.checked_add(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} overflows: {} + {}", context, lhs, rhs)))
}

pub(crate) fn checked_sub_u128(
    lhs: u128,
    rhs: u128,
    context: &str,
) -> Result<u128, ArithmeticError> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} underflows: {} - {}", context, lhs, rhs)))
}

pub(crate) fn checked_mul_u128(
    lhs: u128,
    rhs: u128,
    context: &str,
) -> Result<u128, ArithmeticError> {
    lhs.checked_mul(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} overflows: {} * {}", context, lhs, rhs)))
}

pub(crate) fn checked_sum_u128<I>(values: I, context: &str) -> Result<u128, ArithmeticError>
where
    I: IntoIterator<Item = u128>,
{
    values
        .into_iter()
        .try_fold(0_u128, |sum, value| checked_add_u128(sum, value, context))
}

#[cfg(feature = "watchtower")]
pub(crate) fn checked_sub_usize(
    lhs: usize,
    rhs: usize,
    context: &str,
) -> Result<usize, ArithmeticError> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| ArithmeticError::new(format!("{} underflows: {} - {}", context, lhs, rhs)))
}

pub(crate) fn checked_f64_to_u128(value: f64, context: &str) -> Result<u128, ArithmeticError> {
    if !value.is_finite() || value < 0.0 || value >= u128::MAX as f64 {
        return Err(ArithmeticError::new(format!(
            "{} does not fit into u128: {}",
            context, value
        )));
    }
    Ok(value as u128)
}
