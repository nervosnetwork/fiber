use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::NewInvoiceCommand;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::Hash256;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{add_invoice, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceStore};
use ckb_jsonrpc_types::Script;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::str::FromStr;

const RPC_TIMEOUT_MS: u64 = 3000;

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    pub currency: Option<Currency>,
    /// The payment preimage of the invoice, may be empty for a hold invoice.
    pub payment_preimage: Option<Hash256>,
    /// The payment hash of the invoice, must be given when payment_preimage is empty.
    pub payment_hash: Option<Hash256>,
    /// The expiry time of the invoice.
    #[serde_as(as = "Option<U64Hex>")]
    pub expiry: Option<u64>,
    /// The fallback address of the invoice.
    pub fallback_address: Option<String>,
    /// The final HTLC timeout of the invoice.
    #[serde_as(as = "Option<U64Hex>")]
    pub final_expiry_delta: Option<u64>,
    /// The UDT type script of the invoice.
    pub udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    pub hash_algorithm: Option<HashAlgorithm>,
    /// Whether to save the invoice to the store, default is true.
    pub save_to_store: Option<bool>,
}

impl From<NewInvoiceCommand> for NewInvoiceParams {
    fn from(command: NewInvoiceCommand) -> Self {
        Self {
            amount: command.amount,
            description: command.description,
            currency: command.currency,
            payment_preimage: command.payment_preimage,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            fallback_address: command.fallback_address,
            final_expiry_delta: command.final_expiry_delta,
            udt_type_script: command.udt_type_script.map(Script::from),
            hash_algorithm: command.hash_algorithm,
            save_to_store: command.save_to_store,
        }
    }
}

impl From<NewInvoiceParams> for NewInvoiceCommand {
    fn from(params: NewInvoiceParams) -> Self {
        Self {
            amount: params.amount,
            description: params.description,
            currency: params.currency,
            payment_preimage: params.payment_preimage,
            payment_hash: params.payment_hash,
            expiry: params.expiry,
            fallback_address: params.fallback_address,
            final_expiry_delta: params.final_expiry_delta,
            udt_type_script: params.udt_type_script.map(ckb_types::packed::Script::from),
            hash_algorithm: params.hash_algorithm,
            save_to_store: params.save_to_store,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize)]
pub struct AddInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ParseInvoiceResult {
    /// The invoice.
    pub invoice: CkbInvoice,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SettleInvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SettleInvoiceResult {}

/// The status of the invoice.
#[derive(Clone, Serialize, Deserialize)]
pub struct GetInvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
    /// The invoice status
    pub status: CkbInvoiceStatus,
}

/// RPC module for invoice management.
#[rpc(server)]
trait InvoiceRpc {
    /// Generates a new invoice.
    #[method(name = "new_invoice")]
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned>;

    /// Adds a new invoice to the store.
    #[method(name = "add_invoice")]
    async fn add_invoice(
        &self,
        params: AddInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned>;

    /// Parses a encoded invoice.
    #[method(name = "parse_invoice")]
    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned>;

    /// Retrieves an invoice.
    #[method(name = "get_invoice")]
    async fn get_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Cancels an invoice, only when invoice is in status `Open` can be canceled.
    #[method(name = "cancel_invoice")]
    async fn cancel_invoice(
        &self,
        payment_hash: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Settles an invoice by saving the preimage to this invoice.
    #[method(name = "settle_invoice")]
    async fn settle_invoice(
        &self,
        settle_invoice: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned>;
}

pub(crate) struct InvoiceRpcServerImpl<S> {
    store: S,
    network_actor: ActorRef<NetworkActorMessage>,
}

impl<S> InvoiceRpcServerImpl<S> {
    pub(crate) fn new(store: S, network_actor: ActorRef<NetworkActorMessage>) -> Self {
        Self {
            store,
            network_actor,
        }
    }
}

#[async_trait]
impl<S> InvoiceRpcServer for InvoiceRpcServerImpl<S>
where
    S: InvoiceStore + Send + Sync + 'static,
{
    async fn new_invoice(
        &self,
        params: NewInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::NewInvoice(
                params.clone().into(),
                rpc_reply,
            ))
        };

        call_t!(&self.network_actor, message, RPC_TIMEOUT_MS)
            .map_err(|ractor_error| {
                ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    ractor_error.to_string(),
                    Option::<()>::None,
                )
            })?
            .map_err(|err| {
                ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, err.to_string(), Some(params))
            })
            .map(|invoice| InvoiceResult {
                invoice_address: invoice.to_string(),
                invoice,
            })
    }

    async fn add_invoice(
        &self,
        params: AddInvoiceParams,
    ) -> Result<InvoiceResult, ErrorObjectOwned> {
        let invoice = params.invoice.as_str();
        CkbInvoice::from_str(invoice)
            .and_then(|parsed_invoice| {
                add_invoice(&self.store, parsed_invoice.clone(), None).map(|_| InvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice: parsed_invoice,
                })
            })
            .map_err(|e| {
                ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, e.to_string(), Some(params))
            })
    }

    async fn parse_invoice(
        &self,
        params: ParseInvoiceParams,
    ) -> Result<ParseInvoiceResult, ErrorObjectOwned> {
        let result: Result<CkbInvoice, _> = params.invoice.parse();
        match result {
            Ok(invoice) => Ok(ParseInvoiceResult { invoice }),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }

    async fn get_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash;
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let status = match self
                    .store
                    .get_invoice_status(&payment_hash)
                    .expect("no invoice status found")
                {
                    CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
                    status => status,
                };

                Ok(GetInvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice,
                    status,
                })
            }
            None => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "invoice not found".to_string(),
                Some(payment_hash),
            )),
        }
    }

    async fn cancel_invoice(
        &self,
        params: InvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let payment_hash = params.payment_hash;
        match self.store.get_invoice(&payment_hash) {
            Some(invoice) => {
                let status = match self
                    .store
                    .get_invoice_status(&payment_hash)
                    .expect("no invoice status found")
                {
                    CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
                    status => status,
                };

                let new_status = match status {
                    CkbInvoiceStatus::Paid | CkbInvoiceStatus::Cancelled => {
                        return Err(ErrorObjectOwned::owned(
                            CALL_EXECUTION_FAILED_CODE,
                            format!("invoice can not be canceled, current status: {}", status),
                            Some(payment_hash),
                        ));
                    }
                    _ => CkbInvoiceStatus::Cancelled,
                };
                self.store
                    .update_invoice_status(&payment_hash, new_status)
                    .map_err(|e| {
                        ErrorObjectOwned::owned(
                            CALL_EXECUTION_FAILED_CODE,
                            e.to_string(),
                            Some(payment_hash),
                        )
                    })?;
                Ok(GetInvoiceResult {
                    invoice_address: invoice.to_string(),
                    invoice,
                    status: new_status,
                })
            }
            None => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "invoice not found".to_string(),
                Some(payment_hash),
            )),
        }
    }

    async fn settle_invoice(
        &self,
        params: SettleInvoiceParams,
    ) -> Result<SettleInvoiceResult, ErrorObjectOwned> {
        let SettleInvoiceParams {
            ref payment_hash,
            ref payment_preimage,
        } = params;

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                *payment_hash,
                *payment_preimage,
                rpc_reply,
            ))
        };

        match call_t!(&self.network_actor, message, RPC_TIMEOUT_MS).map_err(|ractor_error| {
            ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                ractor_error.to_string(),
                Option::<()>::None,
            )
        })? {
            Ok(_) => Ok(SettleInvoiceResult {}),
            Err(e) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                e.to_string(),
                Some(params),
            )),
        }
    }
}
