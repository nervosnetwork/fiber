use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::fiber_types::Pubkey;

/// Stable operator-facing identifier for a hosted tenant.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TenantId(String);

impl TenantId {
    /// Construct and validate a tenant identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 {
            return Err("tenant id must contain between 1 and 64 characters".to_string());
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "tenant id may only contain ASCII letters, digits, '-' and '_'".to_string(),
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for TenantId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Persisted identity of a hosted tenant. Runtime liveness is intentionally
/// excluded because it is rebuilt by the supervisor after process restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedTenantRecord {
    pub tenant_id: TenantId,
    pub node_id: Pubkey,
    pub created_at: u64,
}

/// In-process lifecycle of a hosted tenant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantRuntimeStatus {
    Cold,
    Active,
}

/// Combined persistent and in-process status returned by the LSP service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedTenantStatus {
    pub record: HostedTenantRecord,
    pub runtime_status: TenantRuntimeStatus,
}
