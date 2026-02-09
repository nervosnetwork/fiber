
import DbWorker from "./db.worker.ts";
import FiberWorker from "./fiber.worker.ts";
import { Mutex } from "async-mutex";
import { DbWorkerInitializationOptions, FiberInvokeRequest, FiberInvokeResponse, FiberWorkerInitializationOptions } from "./types/general.ts";
import { AbandonChannelParams, AcceptChannelParams, AcceptChannelResult, ListChannelsParams, ListChannelsResult, OpenChannelParams, OpenChannelResult, OpenChannelWithExternalFundingParams, OpenChannelWithExternalFundingResult, ShutdownChannelParams, SubmitSignedFundingTxParams, SubmitSignedFundingTxResult, UpdateChannelParams } from "./types/channel.ts";
import { GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult } from "./types/graph.ts";
import { NodeInfoResult } from "./types/info.ts";
import { GetInvoiceResult, InvoiceParams, InvoiceResult, NewInvoiceParams, ParseInvoiceParams, ParseInvoiceResult } from "./types/invoice.ts";
import { BuildPaymentRouterResult, BuildRouterParams, GetPaymentCommandParams, GetPaymentCommandResult, SendPaymentCommandParams, SendPaymentWithRouterParams } from "./types/payment.ts";
import { ConnectPeerParams, DisconnectPeerParams, ListPeerResult } from "./types/peer.ts";

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
     * @param ckbSecretKey secret key for CKB (optional, signing may not require it)
     * @param chainSpec Chain spec if chain is neither testnet nor mainnet
     * @param logLevel log level, such as `trace`, `debug`, `info`, `error`
     * @param databasePrefix Name prefix of IndexedDB store. Defaults to `/wasm`
     * 
     */
    async start(
        config: string,
        fiberKeyPair: Uint8Array,
        ckbSecretKey?: Uint8Array,
        chainSpec?: string,
        logLevel: "trace" | "debug" | "info" | "error" = "info",
        databasePrefix?: string) {
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
            chainSpec,
            databasePrefix
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
    async openChannel(params: OpenChannelParams): Promise<OpenChannelResult> {
        return await this.invokeCommand("open_channel", [params]);
    }
    async openChannelWithExternalFunding(params: OpenChannelWithExternalFundingParams): Promise<OpenChannelWithExternalFundingResult> {
        return await this.invokeCommand("open_channel_with_external_funding", [params]);
    }
    async submitSignedFundingTx(params: SubmitSignedFundingTxParams): Promise<SubmitSignedFundingTxResult> {
        return await this.invokeCommand("submit_signed_funding_tx", [params]);
    }
    async acceptChannel(params: AcceptChannelParams): Promise<AcceptChannelResult> {
        return await this.invokeCommand("accept_channel", [params]);
    }
    async abandonChannel(params: AbandonChannelParams): Promise<void> {
        await this.invokeCommand("abandon_channel", [params]);
    }
    async listChannels(params: ListChannelsParams): Promise<ListChannelsResult> {
        return await this.invokeCommand("list_channels", [params]);
    }
    async shutdownChannel(params: ShutdownChannelParams): Promise<void> {
        await this.invokeCommand("shutdown_channel", [params]);
    }
    async updateChannel(params: UpdateChannelParams): Promise<void> {
        await this.invokeCommand("update_channel", [params]);
    }
    async graphNodes(params: GraphNodesParams): Promise<GraphNodesResult> {
        return await this.invokeCommand("graph_nodes", [params]);
    }
    async graphChannels(params: GraphChannelsParams): Promise<GraphChannelsResult> {
        return await this.invokeCommand("graph_channels", [params]);
    }
    async nodeInfo(): Promise<NodeInfoResult> {
        return await this.invokeCommand("node_info");
    }
    async newInvoice(params: NewInvoiceParams): Promise<InvoiceResult> {
        return await this.invokeCommand("new_invoice", [params]);
    }
    async parseInvoice(params: ParseInvoiceParams): Promise<ParseInvoiceResult> {
        return await this.invokeCommand("parse_invoice", [params]);
    }
    async getInvoice(params: InvoiceParams): Promise<GetInvoiceResult> {
        return await this.invokeCommand("get_invoice", [params]);
    }
    async cancelInvoice(params: InvoiceParams): Promise<GetInvoiceResult> {
        return await this.invokeCommand("cancel_invoice", [params]);
    }
    async sendPayment(params: SendPaymentCommandParams): Promise<GetPaymentCommandResult> {
        return await this.invokeCommand("send_payment", [params]);
    }
    async getPayment(params: GetPaymentCommandParams): Promise<GetPaymentCommandResult> {
        return await this.invokeCommand("get_payment", [params]);
    }
    async buildRouter(params: BuildRouterParams): Promise<BuildPaymentRouterResult> {
        return await this.invokeCommand("build_router", [params]);
    }
    async sendPaymentWithRouter(params: SendPaymentWithRouterParams): Promise<GetPaymentCommandResult> {
        return await this.invokeCommand("send_payment_with_router", [params]);
    }
    async connectPeer(params: ConnectPeerParams): Promise<void> {
        await this.invokeCommand("connect_peer", [params]);
    }
    async disconnectPeer(params: DisconnectPeerParams): Promise<void> {
        await this.invokeCommand("disconnect_peer", [params]);
    }
    async listPeers(): Promise<ListPeerResult> {
        return await this.invokeCommand("list_peers");
    }
}

export { Fiber };

/**
 * Generate a random 32-byte secret key.
 * @returns The secret key.
 */
export function randomSecretKey(): Uint8Array {
    const arr = new Uint8Array(32);
    crypto.getRandomValues(arr);
    return arr;
}

export * from "./types/general.ts";
export * from "./types/channel.ts";
export * from "./types/graph.ts";
export * from "./types/info.ts";
export * from "./types/invoice.ts";
export * from "./types/payment.ts";
export * from "./types/peer.ts";
