/// <reference path="./yaml.d.ts" />

import { parseDocument } from "yaml";

import mainnetConfig from "../../config/mainnet/config.yml";
import testnetConfig from "../../config/testnet/config.yml";

const DEFAULT_CONFIGS = {
    mainnet: mainnetConfig,
    testnet: testnetConfig
};

type Network = keyof typeof DEFAULT_CONFIGS;

export function getDefaultConfig(network: Network, ckbRpcUrl: string): string {
    // Keep values such as 0x-prefixed hashes as source strings while editing the YAML tree.
    const document = parseDocument(DEFAULT_CONFIGS[network], { schema: "failsafe" });
    document.setIn(["ckb", "rpc_url"], ckbRpcUrl);
    return document.toString({ lineWidth: 0 });
}
