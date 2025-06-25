import { Script } from "./channel";
import { HexString } from "./general"

interface GraphNodesParams {
    limit?: HexString;
    after?: HexString;
}
type UdtScript = Script;
type DepType = "code" | "dep_group";

interface UdtCellDep {
    out_point: OutPoint;
    dep_type: DepType;

}

interface OutPoint {
    tx_hash: HexString;
    index: HexString;
}

interface UdtDep {
    cell_dep?: UdtCellDep;
    type_id?: Script;
}

interface UdpCellDep {
    out_point: OutPoint;
    dep_type: DepType;
}

interface UdtArgInfo {
    name: string;
    script: UdtScript;
    auto_accept_amount?: HexString;
    cell_deps: UdtDep[];
}
type UdtCfgInfos = UdtArgInfo[];

interface NodeInfo {
    node_name: string;
    addresses: HexString[];
    node_id: HexString;
    timestamp: HexString;
    chain_hash: HexString;
    auto_accept_min_ckb_funding_amount: HexString;
    udt_cfg_infos: UdtCfgInfos;
}

interface GraphNodesResult {
    nodes: NodeInfo[];
    last_cursor: HexString;
}
interface GraphChannelsParams {
    limit?: HexString;
    after?: HexString;
}

interface ChannelUpdateInfo {
    timestamp: HexString;
    enabled: boolean;
    outbound_liquidity?: HexString;
    tlc_expiry_delta: HexString;
    tlc_minimum_value: HexString;
    fee_rate: HexString;
}

interface ChannelInfo {
    channel_outpoint: HexString;
    node1: HexString;
    node2: HexString;
    created_timestamp: HexString;
    update_info_of_node1?: ChannelUpdateInfo;
    update_info_of_node2?: ChannelUpdateInfo;
    capacity: HexString;
    chain_hash: HexString;
    udt_type_script?: Script;

}
interface GraphChannelsResult {
    channels: ChannelInfo[];
    last_cursor: HexString;
}
export type {
    GraphNodesParams,
    GraphNodesResult,
    GraphChannelsParams,
    GraphChannelsResult,
    ChannelInfo,
    ChannelUpdateInfo,
    DepType,
    HexString,
    NodeInfo,
    OutPoint,
    Script,
    UdpCellDep,
    UdtArgInfo,
    UdtCellDep,
    UdtCfgInfos,
    UdtDep,
    UdtScript,

}
