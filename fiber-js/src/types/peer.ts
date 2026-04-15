import { Pubkey } from "./general"

type TransportType = "tcp" | "ws" | "wss";

interface ConnectPeerParams {
    address?: string;
    pubkey?: Pubkey;
    save?: boolean;
    addr_type?: TransportType;
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
    TransportType,
    PeerInfo
 }
