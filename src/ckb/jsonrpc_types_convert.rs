use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed,
    prelude::IntoTransactionView as _,
};

pub(super) fn tx_status_from_json(status: ckb_jsonrpc_types::TxStatus) -> TxStatus {
    match status.status {
        ckb_jsonrpc_types::Status::Pending => TxStatus::Pending,
        ckb_jsonrpc_types::Status::Proposed => TxStatus::Proposed,
        ckb_jsonrpc_types::Status::Committed => TxStatus::Committed(
            status.block_number.unwrap_or_default().into(),
            status.block_hash.unwrap_or_default(),
            status.tx_index.unwrap_or_default().into(),
        ),
        ckb_jsonrpc_types::Status::Unknown => TxStatus::Unknown,
        ckb_jsonrpc_types::Status::Rejected => {
            TxStatus::Rejected(status.reason.unwrap_or_default())
        }
    }
}

pub(super) fn transaction_view_from_json(
    tx: ckb_jsonrpc_types::TransactionView,
) -> TransactionView {
    Into::<packed::Transaction>::into(tx.inner).into_view()
}
