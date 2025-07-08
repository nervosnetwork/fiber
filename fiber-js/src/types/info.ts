import { Script } from "./channel";
import { HexString } from "./general";
import { UdtCfgInfos } from "./graph";

interface NodeInfoResult {
    version: string;
    commit_hash: string;
    node_id: HexString;
    node_name?: string;
    addresses: string[];
    chain_hash: HexString;
    open_channel_auto_accept_min_ckb_funding_amount: HexString;
    auto_accept_channel_ckb_funding_amount: HexString;
    default_funding_lock_script: Script;
    tlc_expiry_delta: HexString;
    tlc_min_value: HexString;
    tlc_fee_proportional_millionths: HexString;
    channel_count: HexString;
    pending_channel_count: HexString;
    peers_count: HexString;
    udt_cfg_infos: UdtCfgInfos;
}


export type { NodeInfoResult }
