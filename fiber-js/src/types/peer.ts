import { HexString } from "./general"

interface ConnectPeerParams {
    address: string;
    save?: boolean;

}

interface DisconnectPeerParams {
    peer_id: string;
}

interface PeerInfo {
    pubkey: string;
    peer_id: string;
    address: string;
}

interface ListPeerResult {
    peers: PeerInfo[];
}


export type {
    ConnectPeerParams,
    DisconnectPeerParams,
    ListPeerResult,
    PeerInfo
 }
