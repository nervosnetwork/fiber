use crate::gen::fiber as molecule_fiber;
use crate::primitives::u8_32_as_byte_32;
use ckb_types::packed::{OutPoint, Script};
use ckb_types::prelude::Pack;
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

// ============================================================
// UDT Config Types (for NodeAnnouncement)
// ============================================================

// Serde converters for CKB types
serde_with::serde_conv!(
    ScriptHashTypeWrapper,
    ckb_types::core::ScriptHashType,
    |s: &ckb_types::core::ScriptHashType| -> String {
        use ckb_types::core::ScriptHashType;
        let v = match s {
            ScriptHashType::Type => "type",
            ScriptHashType::Data => "data",
            ScriptHashType::Data1 => "data1",
            ScriptHashType::Data2 => "data2",
            _ => "unknown",
        };
        v.to_string()
    },
    |s: String| {
        use ckb_types::core::ScriptHashType;
        let v = match s.to_lowercase().as_str() {
            "type" => ScriptHashType::Type,
            "data" => ScriptHashType::Data,
            "data1" => ScriptHashType::Data1,
            "data2" => ScriptHashType::Data2,
            _ => return Err("invalid hash type"),
        };
        Ok(v)
    }
);

serde_with::serde_conv!(
    DepTypeWrapper,
    ckb_types::core::DepType,
    |s: &ckb_types::core::DepType| -> String {
        use ckb_types::core::DepType;
        let v = match s {
            DepType::Code => "code",
            DepType::DepGroup => "dep_group",
        };
        v.to_string()
    },
    |s: String| {
        let v = match s.to_lowercase().as_str() {
            "code" => ckb_types::core::DepType::Code,
            "dep_group" => ckb_types::core::DepType::DepGroup,
            _ => return Err("invalid dep type"),
        };
        Ok(v)
    }
);

/// UDT argument information.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtArgInfo {
    /// Name of the UDT
    pub name: String,
    /// UDT script configuration
    pub script: UdtScript,
    /// Auto-accept amount for this UDT
    pub auto_accept_amount: Option<u128>,
    /// Cell dependencies for this UDT
    pub cell_deps: Vec<UdtDep>,
}

/// UDT script configuration.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Default)]
pub struct UdtScript {
    /// Code hash of the UDT script
    pub code_hash: ckb_types::H256,
    /// Hash type of the UDT script
    #[serde_as(as = "ScriptHashTypeWrapper")]
    pub hash_type: ckb_types::core::ScriptHashType,
    /// Arguments for the UDT script
    pub args: String,
}

/// UDT cell dependency configuration.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtCellDep {
    /// The outpoint of the cell dependency
    pub out_point: ckb_jsonrpc_types::OutPoint,
    /// The dependency type
    #[serde_as(as = "DepTypeWrapper")]
    pub dep_type: ckb_types::core::DepType,
}

/// UDT dependency (either cell dep or type ID).
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct UdtDep {
    /// Cell dependency (if using direct reference)
    #[serde(default)]
    pub cell_dep: Option<UdtCellDep>,
    /// Type ID script (if using type ID reference)
    #[serde(default)]
    pub type_id: Option<ckb_jsonrpc_types::Script>,
}

impl UdtDep {
    /// Create a UdtDep with a cell dependency.
    pub fn with_cell_dep(cell_dep: UdtCellDep) -> Self {
        Self {
            cell_dep: Some(cell_dep),
            type_id: None,
        }
    }

    /// Create a UdtDep with a type ID script.
    pub fn with_type_id(type_id: ckb_jsonrpc_types::Script) -> Self {
        Self {
            cell_dep: None,
            type_id: Some(type_id),
        }
    }
}

/// Collection of UDT configuration information.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Default)]
pub struct UdtCfgInfos(pub Vec<UdtArgInfo>);

impl std::str::FromStr for UdtCfgInfos {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

// UdtCfgInfos molecule conversions

impl From<UdtCfgInfos> for molecule_fiber::UdtCfgInfos {
    fn from(udt_cfg_infos: UdtCfgInfos) -> Self {
        molecule_fiber::UdtCfgInfos::new_builder()
            .set(
                udt_cfg_infos
                    .0
                    .into_iter()
                    .map(|udt_arg_info| udt_arg_info.into())
                    .collect(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::UdtCfgInfos> for UdtCfgInfos {
    type Error = anyhow::Error;

    fn try_from(udt_cfg_infos: molecule_fiber::UdtCfgInfos) -> Result<Self, Self::Error> {
        Ok(UdtCfgInfos(
            udt_cfg_infos
                .into_iter()
                .map(|udt_arg_info| udt_arg_info.try_into())
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }
}

// UdtArgInfo molecule conversions

impl From<UdtArgInfo> for molecule_fiber::UdtArgInfo {
    fn from(udt_arg_info: UdtArgInfo) -> Self {
        let builder = molecule_fiber::UdtArgInfo::new_builder()
            .name(udt_arg_info.name.pack())
            .script(udt_arg_info.script.into())
            .cell_deps(
                molecule_fiber::UdtCellDeps::new_builder()
                    .set(udt_arg_info.cell_deps.into_iter().map(Into::into).collect())
                    .build(),
            );
        let builder = if let Some(amount) = udt_arg_info.auto_accept_amount {
            builder.auto_accept_amount(
                molecule_fiber::Uint128Opt::new_builder()
                    .set(Some(amount.pack()))
                    .build(),
            )
        } else {
            builder
        };
        builder.build()
    }
}

impl TryFrom<molecule_fiber::UdtArgInfo> for UdtArgInfo {
    type Error = anyhow::Error;

    fn try_from(udt_arg_info: molecule_fiber::UdtArgInfo) -> Result<Self, Self::Error> {
        use ckb_types::prelude::Unpack;
        Ok(UdtArgInfo {
            name: String::from_utf8(udt_arg_info.name().unpack()).unwrap_or_default(),
            script: udt_arg_info.script().try_into()?,
            auto_accept_amount: udt_arg_info
                .auto_accept_amount()
                .to_opt()
                .map(|amount| amount.unpack()),
            cell_deps: udt_arg_info
                .cell_deps()
                .into_iter()
                .map(|cell_dep| cell_dep.try_into())
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

// UdtScript molecule conversions

impl From<UdtScript> for molecule_fiber::UdtScript {
    fn from(udt_script: UdtScript) -> Self {
        use ckb_types::core::ScriptHashType;
        let code_hash_bytes: [u8; 32] = udt_script.code_hash.into();
        molecule_fiber::UdtScript::new_builder()
            .code_hash(u8_32_as_byte_32(&code_hash_bytes))
            .hash_type(
                match udt_script.hash_type {
                    ScriptHashType::Type => 1u8,
                    ScriptHashType::Data => 0u8,
                    ScriptHashType::Data1 => 2u8,
                    ScriptHashType::Data2 => 4u8,
                    _ => panic!("unsupported hash type: {:?}", udt_script.hash_type),
                }
                .into(),
            )
            .args(udt_script.args.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::UdtScript> for UdtScript {
    type Error = anyhow::Error;

    fn try_from(udt_script: molecule_fiber::UdtScript) -> Result<Self, Self::Error> {
        use ckb_types::core::ScriptHashType;
        use ckb_types::prelude::Unpack;
        let hash_type: u8 = udt_script.hash_type().into();
        Ok(UdtScript {
            code_hash: ckb_types::H256::from_slice(udt_script.code_hash().as_slice())?,
            hash_type: match hash_type {
                0 => ScriptHashType::Data,
                1 => ScriptHashType::Type,
                2 => ScriptHashType::Data1,
                4 => ScriptHashType::Data2,
                _ => return Err(anyhow::anyhow!("Invalid hash_type: {}", hash_type)),
            },
            args: String::from_utf8(udt_script.args().unpack()).unwrap_or_default(),
        })
    }
}

// UdtDep molecule conversions

impl From<UdtDep> for molecule_fiber::UdtDep {
    fn from(udt_dep: UdtDep) -> Self {
        match udt_dep {
            UdtDep {
                cell_dep: Some(cell_dep),
                type_id: None,
            } => molecule_fiber::UdtDep::new_builder()
                .set(molecule_fiber::UdtDepUnion::UdtCellDep(cell_dep.into()))
                .build(),
            UdtDep {
                cell_dep: None,
                type_id: Some(type_id),
            } => molecule_fiber::UdtDep::new_builder()
                .set(molecule_fiber::UdtDepUnion::Script(Script::from(type_id)))
                .build(),
            _ => panic!("UdtDep must have exactly one of cell_dep or type_id"),
        }
    }
}

impl TryFrom<molecule_fiber::UdtDep> for UdtDep {
    type Error = anyhow::Error;

    fn try_from(udt_dep: molecule_fiber::UdtDep) -> Result<Self, Self::Error> {
        match udt_dep.to_enum() {
            molecule_fiber::UdtDepUnion::UdtCellDep(cell_dep) => Ok(UdtDep {
                cell_dep: Some(cell_dep.try_into()?),
                type_id: None,
            }),
            molecule_fiber::UdtDepUnion::Script(type_id) => Ok(UdtDep {
                cell_dep: None,
                type_id: Some(type_id.into()),
            }),
        }
    }
}

// UdtCellDep molecule conversions

impl From<UdtCellDep> for molecule_fiber::UdtCellDep {
    fn from(udt_cell_dep: UdtCellDep) -> Self {
        use molecule::prelude::Entity;
        molecule_fiber::UdtCellDep::new_builder()
            .out_point(OutPoint::from(udt_cell_dep.out_point))
            .dep_type(
                match udt_cell_dep.dep_type {
                    ckb_types::core::DepType::Code => 0u8,
                    ckb_types::core::DepType::DepGroup => 1u8,
                }
                .into(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::UdtCellDep> for UdtCellDep {
    type Error = anyhow::Error;

    fn try_from(udt_cell_dep: molecule_fiber::UdtCellDep) -> Result<Self, Self::Error> {
        let dep_type: u8 = udt_cell_dep.dep_type().into();
        Ok(UdtCellDep {
            out_point: udt_cell_dep.out_point().into(),
            dep_type: match dep_type {
                0 => ckb_types::core::DepType::Code,
                1 => ckb_types::core::DepType::DepGroup,
                _ => return Err(anyhow::anyhow!("Invalid dep_type: {}", dep_type)),
            },
        })
    }
}

impl From<UdtCellDep> for ckb_types::packed::CellDep {
    fn from(udt_cell_dep: UdtCellDep) -> Self {
        let out_point: ckb_types::packed::OutPoint = udt_cell_dep.out_point.into();
        ckb_types::packed::CellDep::new_builder()
            .out_point(out_point)
            .dep_type(udt_cell_dep.dep_type)
            .build()
    }
}

impl From<&UdtCellDep> for ckb_types::packed::CellDep {
    fn from(udt_cell_dep: &UdtCellDep) -> Self {
        let out_point: ckb_types::packed::OutPoint = udt_cell_dep.out_point.clone().into();
        ckb_types::packed::CellDep::new_builder()
            .out_point(out_point)
            .dep_type(udt_cell_dep.dep_type)
            .build()
    }
}

impl TryFrom<ckb_types::packed::CellDep> for UdtCellDep {
    type Error = anyhow::Error;

    fn try_from(cell_dep: ckb_types::packed::CellDep) -> Result<Self, Self::Error> {
        let dep_type: u8 = cell_dep.dep_type().into();
        Ok(UdtCellDep {
            out_point: cell_dep.out_point().into(),
            dep_type: match dep_type {
                0 => ckb_types::core::DepType::Code,
                1 => ckb_types::core::DepType::DepGroup,
                _ => return Err(anyhow::anyhow!("Invalid dep_type: {}", dep_type)),
            },
        })
    }
}
