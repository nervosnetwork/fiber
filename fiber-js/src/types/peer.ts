import { HexString } from "./general"

interface ConnectPeerParams {
    address: HexString;
    save?: boolean;

}

interface DisconnectPeerParams {
    peer_id: HexString;
}

interface PeerInfo {
    pubkey: HexString;
    peer_id: HexString;
    addresses: HexString[];
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
