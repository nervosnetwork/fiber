import { Script } from "./channel";
import { HexString } from "./general"

type Currency = "Fibb" | "Fibt" | "Fibd";
type CkbInvoiceStatus = "Open" | "Cancelled" | "Expired" | "Received" | "Paid";
interface NewInvoiceParams {
    amount: HexString;
    description?: string;
    currency: Currency;
    payment_preimage: HexString;
    expiry?: HexString;
    fallback_address?: string;
    final_expiry_delta?: HexString;
    udt_type_script?: Script;
    hash_algorithm?: number;
}
type CkbScript = Script;
type Attribute = { FinalHtlcTimeout: HexString } |
{ FinalHtlcMinimumExpiryDelta: HexString } |
{ ExpiryTime: HexString } |
{ Description: string } |
{ FallbackAddr: string } |
{ UdtScript: CkbScript } |
{ PayeePublicKey: HexString } |
{ HashAlgorithm: number } |
{ Feature: HexString };


interface InvoiceData {
    timestamp: HexString;
    payment_hash: HexString;
    attrs: Attribute[];
}
interface CkbInvoice {
    currency: Currency;
    amount?: HexString;
    signature?: HexString;
    data: InvoiceData;
}

interface InvoiceResult {
    invoice_address: string;
    invoice: CkbInvoice;
}

interface ParseInvoiceParams {
    invoice: string;
}
interface ParseInvoiceResult {
    invoice: CkbInvoice;
}
interface InvoiceParams {
    payment_hash: HexString;
}

interface GetInvoiceResult {
    invoice_address: string;
    invoice: CkbInvoice;
    status: CkbInvoiceStatus;
}

export type {
    NewInvoiceParams,
    InvoiceResult,
    Attribute,
    CkbInvoice,
    CkbInvoiceStatus,
    CkbScript,
    Currency,
    GetInvoiceResult,
    InvoiceData,
    InvoiceParams,
    ParseInvoiceParams,
    ParseInvoiceResult
}
