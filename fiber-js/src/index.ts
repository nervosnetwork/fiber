
import DbWorker from "./db.worker.ts";
import FiberWorker from "./fiber.worker.ts";
import { Mutex } from "async-mutex";
import { DbWorkerInitializationOptions, FiberInvokeRequest, FiberInvokeResponse, FiberWorkerInitializationOptions } from "./types.ts";

const DEFAULT_BUFFER_SIZE = 50 * (1 << 20);
/**
 * A Fiber Wasm instance
 */
class Fiber {
    private dbWorker: Worker | null
    private fiberWorker: Worker | null
    private inputBuffer: SharedArrayBuffer
    private outputBuffer: SharedArrayBuffer
    private commandInvokeLock: Mutex;
    /**
     * Construct a Fiber Wasm instance.
     * inputBuffer and outputBuffer are buffers used for transporting data between database and fiber wasm. Set them to appropriate sizes.
     * @param inputBufferSize Size of inputBuffer
     * @param outputBufferSize Size of outputBuffer
     */
    constructor(inputBufferSize = DEFAULT_BUFFER_SIZE, outputBufferSize = DEFAULT_BUFFER_SIZE) {
        this.dbWorker = new DbWorker();
        this.fiberWorker = new FiberWorker();
        this.inputBuffer = new SharedArrayBuffer(inputBufferSize);
        this.outputBuffer = new SharedArrayBuffer(outputBufferSize);
        this.commandInvokeLock = new Mutex();
    }

    /**
     * Start the Fiber Wasm instance.
     * @param config Config file for fiber
     * @param fiberKeyPair keypair used for fiber
     * @param ckbSecretKey secret key for CKB
     * @param chainSpec Chain spec if chain is neither testnet nor mainnet
     * @param logLevel log level, such as `trace`, `debug`, `info`, `error`
     * 
     */
    async start(config: string, fiberKeyPair: Uint8Array, ckbSecretKey: Uint8Array, chainSpec?: string, logLevel: "trace" | "debug" | "info" | "error" = "info") {
        this.dbWorker.postMessage({
            inputBuffer: this.inputBuffer,
            outputBuffer: this.outputBuffer,
            logLevel: logLevel
        } as DbWorkerInitializationOptions);
        this.fiberWorker.postMessage({
            inputBuffer: this.inputBuffer,
            outputBuffer: this.outputBuffer,
            ckbSecretKey,
            config,
            fiberKeyPair,
            logLevel,
            chainSpec
        } as FiberWorkerInitializationOptions);
        await new Promise<void>((res, rej) => {
            this.dbWorker.onmessage = () => res();
            this.dbWorker.onerror = (evt) => rej(evt);
        });
        await new Promise<void>((res, rej) => {
            this.fiberWorker.onmessage = () => res();
            this.fiberWorker.onerror = (evt) => rej(evt);
        });

    }
    invokeCommand(name: string, args?: any[]): Promise<any> {
        // Why use lock here?
        // fiber-wasm provided async APIs. Use lock here to avoid mixture of different calls
        return this.commandInvokeLock.runExclusive(async () => {
            this.fiberWorker.postMessage({
                name,
                args: args || []
            } as FiberInvokeRequest);
            return await new Promise((resolve, reject) => {
                const clean = () => {
                    this.fiberWorker.removeEventListener("message", resolveFn);
                    this.fiberWorker.removeEventListener("error", errorFn);
                }
                const resolveFn = (evt: MessageEvent<FiberInvokeResponse>) => {
                    if (evt.data.ok === true) {
                        resolve(evt.data.data);
                    } else {
                        reject(evt.data.error);
                    }
                    clean();

                };
                const errorFn = (evt: ErrorEvent) => {
                    reject(evt);
                    clean();

                };
                this.fiberWorker.addEventListener("message", resolveFn);
                this.fiberWorker.addEventListener("error", errorFn);
            })
        })

    }
    /**
     * Stop the fiber instance.
     */
    async stop() {
        this.dbWorker.terminate();
        this.fiberWorker.terminate();
    }
    // /**
    //  * Returns the header with the highest block number in the canonical chain
    //  * @returns HeaderView
    //  */
    // async getTipHeader(): Promise<ClientBlockHeader> {
    //     return JsonRpcTransformers.blockHeaderTo(await this.invokeLightClientCommand("get_tip_header"));
    // }
    // /**
    //  * Returns the genesis block
    //  * @returns BlockView
    //  */
    // async getGenesisBlock(): Promise<ClientBlock> {
    //     return JsonRpcTransformers.blockTo(await this.invokeLightClientCommand("get_genesis_block"));
    // }
    // /**
    //  * Returns the information about a block header by hash.
    //  * @param hash the block hash, equal to Vec<u8> in Rust
    //  * @returns HeaderView
    //  */
    // async getHeader(hash: HexLike): Promise<ClientBlockHeader | undefined> {
    //     const resp = await this.invokeLightClientCommand("get_header", [hexFrom(hash)]);
    //     return resp ? JsonRpcTransformers.blockHeaderTo(resp) : resp;
    // }
    // /**
    //  * Fetch a header from remote node. If return status is not_found will re-sent fetching request immediately.
    //  * @param hash the block hash, equal to Vec<u8> in Rust
    //  * @returns FetchHeaderResponse
    //  */
    // async fetchHeader(hash: HexLike): Promise<FetchResponse<ClientBlockHeader>> {
    //     return transformFetchResponse(await this.invokeLightClientCommand("fetch_header", [hexFrom(hash)]), (arg: JsonRpcBlockHeader) => JsonRpcTransformers.blockHeaderTo(arg));
    // }
    // /**
    //  * See https://github.com/nervosnetwork/ckb/tree/develop/rpc#method-estimate_cycles
    //  * @param tx The transaction
    //  * @returns Estimate cycles
    //  */
    // async estimateCycles(tx: TransactionLike): Promise<Num> {
    //     return numFrom((await this.invokeLightClientCommand("estimate_cycles", [JsonRpcTransformers.transactionFrom(tx)]) as { cycles: Hex }).cycles);
    // }
    // /**
    //  * Returns the local node information.
    //  * @returns LocalNode
    //  */
    // async localNodeInfo(): Promise<LocalNode> {
    //     return localNodeTo(await this.invokeLightClientCommand("local_node_info") as LightClientLocalNode);
    // }
    // /**
    //  * Returns the connected peers' information.
    //  * @returns 
    //  */
    // async getPeers(): Promise<RemoteNode[]> {
    //     return (await this.invokeLightClientCommand("get_peers") as LightClientRemoteNode[]).map(x => remoteNodeTo(x));
    // }
    // /**
    //  * Set some scripts to filter
    //  * @param scripts Array of script status
    //  * @param command An optional enum parameter to control the behavior of set_scripts
    //  */
    // async setScripts(scripts: ScriptStatus[], command?: LightClientSetScriptsCommand): Promise<void> {
    //     await this.invokeLightClientCommand("set_scripts", [scripts.map(x => scriptStatusFrom(x)), command]);
    // }
    // /**
    //  * Get filter scripts status
    //  */
    // async getScripts(): Promise<ScriptStatus[]> {
    //     return (await this.invokeLightClientCommand("get_scripts") as LightClientScriptStatus[]).map(x => scriptStatusTo(x));
    // }
    // /**
    //  * See https://github.com/nervosnetwork/ckb-indexer#get_cells
    //  * @param searchKey 
    //  * @param order 
    //  * @param limit 
    //  * @param afterCursor 
    //  */
    // async getCells(
    //     searchKey: ClientIndexerSearchKeyLike,
    //     order?: "asc" | "desc",
    //     limit?: NumLike,
    //     afterCursor?: Hex
    // ): Promise<GetCellsResponse> {
    //     const resp = await this.invokeLightClientCommand("get_cells", [
    //         JsonRpcTransformers.indexerSearchKeyFrom(searchKey),
    //         cccOrderToLightClientWasmOrder(order ?? "asc"),
    //         Number(numFrom(numToHex(limit ?? 10))),
    //         afterCursor ? bytesFrom(afterCursor) : afterCursor
    //     ]);
    //     return getCellsResponseFrom(resp);
    // }
    // /**
    //  * See https://github.com/nervosnetwork/ckb-indexer#get_transactions
    //  * @param searchKey 
    //  * @param order 
    //  * @param limit 
    //  * @param afterCursor 
    //  * @returns 
    //  */
    // async getTransactions(
    //     searchKey: ClientIndexerSearchKeyTransactionLike,
    //     order?: "asc" | "desc",
    //     limit?: NumLike,
    //     afterCursor?: Hex
    // ): Promise<GetTransactionsResponse<TxWithCell> | GetTransactionsResponse<TxWithCells>> {
    //     return lightClientGetTransactionsResultTo(await this.invokeLightClientCommand(
    //         "get_transactions",
    //         [
    //             JsonRpcTransformers.indexerSearchKeyTransactionFrom(searchKey),
    //             cccOrderToLightClientWasmOrder(order ?? "asc"),
    //             Number(numFrom(numToHex(limit ?? 10))),
    //             afterCursor ? bytesFrom(afterCursor) : afterCursor
    //         ]
    //     ));
    // }
    // /**
    //  * See https://github.com/nervosnetwork/ckb-indexer#get_cells_capacity
    //  * @param searchKey 
    //  * @returns 
    //  */
    // async getCellsCapacity(searchKey: ClientIndexerSearchKeyLike): Promise<Num> {
    //     return numFrom(((await this.invokeLightClientCommand("get_cells_capacity", [JsonRpcTransformers.indexerSearchKeyFrom(searchKey)])) as any).capacity);
    // }
    // /**
    //  * Submits a new transaction and broadcast it to network peers
    //  * @param tx Transaction
    //  * @returns H256
    //  */
    // async sendTransaction(tx: TransactionLike): Promise<Hex> {
    //     return hexFrom(await this.invokeLightClientCommand("send_transaction", [JsonRpcTransformers.transactionFrom(tx)]));
    // }
    // /**
    //  * Returns the information about a transaction by hash, the block header is also returned.
    //  * @param txHash the transaction hash
    //  * @returns 
    //  */
    // async getTransaction(txHash: HexLike): Promise<ClientTransactionResponse | undefined> {
    //     return JsonRpcTransformers.transactionResponseTo(await this.invokeLightClientCommand("get_transaction", [hexFrom(txHash)]));
    // }
    // /**
    //  * Fetch a transaction from remote node. If return status is not_found will re-sent fetching request immediately.
    //  * @param txHash the transaction hash
    //  * @returns 
    //  */
    // async fetchTransaction(txHash: HexLike): Promise<FetchResponse<ClientTransactionResponse>> {
    //     return transformFetchResponse<any, ClientTransactionResponse>(await this.invokeLightClientCommand("fetch_transaction", [hexFrom(txHash)]), JsonRpcTransformers.transactionResponseTo);
    // }

}

export { Fiber };

/**
 * Generate a random network secret key.
 * @returns The secret key.
 */
// export function randomSecretKey(): Hex {
//     const arr = new Uint8Array(32);
//     crypto.getRandomValues(arr);
//     return hexFrom(arr);
// }

export * from "./types.ts";
