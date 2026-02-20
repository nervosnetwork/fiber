import { HexString } from "./general";

interface Script {
    code_hash: HexString;
    hash_type: "data" | "type" | "data1" | "data2";
    args: string;
}

interface OpenChannelParams {
    peer_id: string;
    funding_amount: HexString;
    public?: boolean;
    funding_udt_type_script?: Script;
    shutdown_script?: Script;
    commitment_delay_epoch?: HexString;
    commitment_fee_rate?: HexString;
    funding_fee_rate?: HexString;
    tlc_expiry_delta?: HexString;
    tlc_min_value?: HexString;
    tlc_fee_proportional_millionths?: HexString;
    max_tlc_value_in_flight?: HexString;
    max_tlc_number_in_flight?: HexString;
}
interface OpenChannelResult {
    temporary_channel_id: HexString;
}
interface AbandonChannelParams {
    channel_id: HexString;
}
interface AcceptChannelParams {
    temporary_channel_id: HexString;
    funding_amount: HexString;
    shutdown_script?: Script;
    max_tlc_value_in_flight?: HexString;
    max_tlc_number_in_flight?: HexString;
    tlc_min_value?: HexString;
    tlc_fee_proportional_millionths?: HexString;
    tlc_expiry_delta?: HexString;
}
interface AcceptChannelResult {
    channel_id: HexString;
}
interface ListChannelsParams {
    peer_id?: string;
    include_closed?: boolean;
}

interface ChannelState {
    state_name: string;
    state_flags: string;
}
interface Channel {
    channel_id: HexString;
    is_public: boolean;
    channel_outpoint: HexString;
    peer_id: HexString;
    funding_udt_type_script?: Script;
    state: ChannelState;
    local_balance: HexString;
    offered_tlc_balance: HexString;
    remote_balance: HexString;
    received_tlc_balance: HexString;
    latest_commitment_transaction_hash?: HexString;
    created_at: HexString;
    enabled: boolean;
    tlc_expiry_delta: HexString;
    tlc_fee_proportional_millionths: HexString;
    shutdown_transaction_hash?: HexString;
}

interface ShutdownChannelParams {
    channel_id: HexString;
    close_script?: Script;
    fee_rate?: HexString;
    force?: boolean;
}

interface UpdateChannelParams {
    channel_id: HexString;
    enabled?: boolean;
    tlc_expiry_delta?: HexString;
    tlc_minimum_value?: HexString;
    tlc_fee_proportional_millionths?: HexString;
}
interface ListChannelsResult {
    channels: Channel[];
}
export type {
    OpenChannelParams,
    Script,
    OpenChannelResult,
    AbandonChannelParams,
    AcceptChannelParams,
    AcceptChannelResult,
    ListChannelsParams,
    Channel,
    ChannelState,
    ShutdownChannelParams,
    UpdateChannelParams,
    ListChannelsResult
}
