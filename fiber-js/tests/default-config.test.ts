import { expect, test } from "@jest/globals";
import { parseDocument } from "yaml";

import mainnetConfig from "../../config/mainnet/config.yml";
import testnetConfig from "../../config/testnet/config.yml";
import { getDefaultConfig } from "../src/default-config.ts";

const ckbRpcUrl = "https://example.com/rpc?token=a:#\"$&";

function formatConfigWithCkbRpcUrl(source: string): string {
    const replaced = source.replace(
        /^([ \t]*rpc_url:[ \t]*).*$/m,
        (_match, prefix: string) => `${prefix}${JSON.stringify(ckbRpcUrl)}`
    );
    return parseDocument(replaced, { schema: "failsafe" }).toString({ lineWidth: 0 });
}

test("getDefaultConfig returns the bundled mainnet config with the requested CKB RPC URL", () => {
    expect(getDefaultConfig("mainnet", ckbRpcUrl)).toBe(
        formatConfigWithCkbRpcUrl(mainnetConfig)
    );
});

test("getDefaultConfig returns the bundled testnet config with the requested CKB RPC URL", () => {
    expect(getDefaultConfig("testnet", ckbRpcUrl)).toBe(
        formatConfigWithCkbRpcUrl(testnetConfig)
    );
});
