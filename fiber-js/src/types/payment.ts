import { Script } from "./channel";
import { HexString } from "./general"
type PaymentSessionStatus = "Created" | "Inflight" | "Success" | "Failed";

interface PaymentCustomRecords {
    [k: HexString]: HexString;
}
interface SessionRouteNode {
    pubkey: string;
    amount: HexString;
    channel_outpoint: HexString;
}
interface GetPaymentCommandResult {
    payment_hash: HexString;
    status: PaymentSessionStatus;
    created_at: HexString;
    last_updated_at: HexString;
    failed_error?: string;
    fee: HexString;
    custom_records?: PaymentCustomRecords;
    /// Only available in debug mode
    router?: SessionRouteNode[];
}
interface HopHint {
    pubkey: string;
    channel_outpoint: HexString;
    fee_rate: HexString;
    tlc_expiry_delta: HexString;
}
interface HopRequire {
    pubkey: string;
    channel_outpoint: HexString;
}
interface RouterHop {
    target: HexString;
    channel_outpoint: HexString;
    amount_received: HexString;
    incoming_tlc_expiry: HexString;
}
interface GetPaymentCommandParams {
    payment_hash: HexString;
}
interface SendPaymentCommandParams {
    target_pubkey?: string;
    amount?: HexString;
    payment_hash?: HexString;
    final_tlc_expiry_delta?: HexString;
    tlc_expiry_limit?: HexString;
    invoice?: string;
    timeout?: HexString;
    max_fee_amount?: HexString;
    max_fee_rate?: HexString;
    max_parts?: HexString;
    trampoline_hops?: string[];
    keysend?: boolean;
    udt_type_script?: Script;
    allow_self_payment?: boolean;
    custom_records?: PaymentCustomRecords;
    hop_hints?: HopHint[];
    dry_run?: boolean;
}
interface BuildRouterParams {
    amount?: HexString;
    udt_type_script?: Script;
    hops_info: HopRequire[];
    final_tlc_expiry_delta?: HexString;
}
interface BuildPaymentRouterResult {
    router_hops: RouterHop[];
}
interface SendPaymentWithRouterParams {
    payment_hash?: HexString;
    router: RouterHop[];
    invoice?: string;
    custom_records?: PaymentCustomRecords;
    keysend?: boolean;
    udt_type_script?: Script;
    dry_run?: boolean;
}
export type {
    GetPaymentCommandResult,
    BuildPaymentRouterResult,
    BuildRouterParams,
    HopHint,
    HopRequire,
    PaymentCustomRecords,
    PaymentSessionStatus,
    RouterHop,
    SendPaymentCommandParams,
    SendPaymentWithRouterParams,
    SessionRouteNode,
    GetPaymentCommandParams
}
