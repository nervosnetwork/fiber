import { HexString } from "./general"

interface ConnectPeerParams {
    address: string;
    save?: boolean;

}

interface DisconnectPeerParams {
    pubkey: HexString;
}

interface PeerInfo {
    pubkey: HexString;
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
