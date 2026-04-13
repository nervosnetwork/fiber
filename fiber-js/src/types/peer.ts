import { Pubkey } from "./general"

type MultiAddrType = "tcp" | "ws" | "wss";

interface ConnectPeerParams {
    address?: string;
    pubkey?: Pubkey;
    save?: boolean;
    addr_type?: MultiAddrType;
}

interface DisconnectPeerParams {
    pubkey: Pubkey;
}

interface PeerInfo {
    pubkey: Pubkey;
    address: string;
}

interface ListPeerResult {
    peers: PeerInfo[];
}


export type {
    ConnectPeerParams,
    DisconnectPeerParams,
    ListPeerResult,
    MultiAddrType,
    PeerInfo
 }
