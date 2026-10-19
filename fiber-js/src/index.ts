
import DbWorker from "./db.worker.ts";
import FiberWorker from "./fiber.worker.ts";
import { Mutex } from "async-mutex";
import { getDefaultConfig } from "./default-config.ts";
import { DbWorkerInitializationOptions, FiberInvokeRequest, FiberInvokeResponse, FiberWorkerInitializationOptions } from "./types/general.ts";
import { AbandonChannelParams, AcceptChannelParams, AcceptChannelResult, ListChannelsParams, ListChannelsResult, OpenChannelParams, OpenChannelResult, OpenChannelWithExternalFundingParams, OpenChannelWithExternalFundingResult, ShutdownChannelParams, SubmitSignedFundingTxParams, SubmitSignedFundingTxResult, UpdateChannelParams } from "./types/channel.ts";
import { GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult } from "./types/graph.ts";
import { NodeInfoResult } from "./types/info.ts";
import { GetInvoiceResult, InvoiceParams, InvoiceResult, NewInvoiceParams, ParseInvoiceParams, ParseInvoiceResult } from "./types/invoice.ts";
import { BuildPaymentRouterResult, BuildRouterParams, GetPaymentCommandParams, GetPaymentCommandResult, SendPaymentCommandParams, SendPaymentWithRouterParams } from "./types/payment.ts";
import { ConnectPeerParams, DisconnectPeerParams, ListPeerResult } from "./types/peer.ts";

const DEFAULT_BUFFER_SIZE = 50 * (1 << 20);
const STOP_TIMEOUT_MS = 30_000;
/**
 * A Fiber Wasm instance
 */
class Fiber {
    private dbWorker: Worker
    private fiberWorker: Worker
    private inputBuffer: SharedArrayBuffer
    private outputBuffer: SharedArrayBuffer
    private commandInvokeLock: Mutex;
    private startPromise?: Promise<void>;
    private stopPromise?: Promise<void>;
    private workerFailure?: Error;
    private readonly workerAbort = new AbortController();
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
        for (const worker of [this.dbWorker, this.fiberWorker]) {
            worker.addEventListener("message", (event: MessageEvent) => {
                if (event.data?.type === "error") {
                    this.workerFailure = new Error(event.data.error);
                }
            });
            worker.addEventListener("error", (event: ErrorEvent) => {
                this.workerFailure = new Error(event.message || "Fiber worker failed");
            });
            worker.addEventListener("messageerror", () => {
                this.workerFailure = new Error("Unable to deserialize Fiber worker response");
            });
        }
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
    start(
        config: string,
        fiberKeyPair: Uint8Array,
        ckbSecretKey?: Uint8Array,
        chainSpec?: string,
        logLevel: "trace" | "debug" | "info" | "error" = "info",
        databasePrefix?: string): Promise<void> {
        if (this.startPromise || this.stopPromise) {
            return Promise.reject(new Error("Fiber has already been started or stopped"));
        }
        this.startPromise = (async () => {
            // Register both listeners before either worker can reply.
            const ready = Promise.all([
                this.waitForMessage(this.dbWorker, (data) => data?.type === "ready"),
                this.waitForMessage(this.fiberWorker, (data) => data?.type === "ready")
            ]);
            this.dbWorker.postMessage({
                inputBuffer: this.inputBuffer,
                outputBuffer: this.outputBuffer,
                logLevel
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
            await ready;
        })();
        return this.startPromise;
    }

    /**
     * Build a browser-compatible mainnet or testnet configuration.
     * @param network Fiber network whose bundled configuration should be used
     * @param ckbRpcUrl Trusted CKB RPC endpoint used by the browser
     */
    getDefaultConfig(network: "mainnet" | "testnet", ckbRpcUrl: string): string {
        return getDefaultConfig(network, ckbRpcUrl);
    }

    invokeCommand(name: string, args?: any[]): Promise<any> {
        if (this.stopPromise) {
            return Promise.reject(new Error("Fiber is stopping or stopped"));
        }
        if (!this.startPromise) {
            return Promise.reject(new Error("Fiber not started"));
        }
        // Why use lock here?
        // fiber-wasm provided async APIs. Use lock here to avoid mixture of different calls
        return this.commandInvokeLock.runExclusive(async () => {
            await this.startPromise;
            return this.invokeWorker(name, args);
        })

    }
    /**
     * Stop the Fiber instance after actors, background writes, and IndexedDB
     * transactions finish. Once resolved, its database can be safely exported.
     * Rejects if shutdown or a database operation failed. After 30 seconds,
     * forcefully terminate both workers and reject without a safe-backup guarantee.
     */
    stop(): Promise<void> {
        if (!this.stopPromise) {
            let timer: ReturnType<typeof setTimeout>;
            // Start the deadline before acquiring the command lock: a stuck API
            // call or startup must not prevent the forced-shutdown fallback.
            const timeout = new Promise<never>((_, reject) => {
                timer = setTimeout(() => reject(new Error(
                    "Fiber stop timed out after 30 seconds; workers terminated, safe backup is not guaranteed"
                )), STOP_TIMEOUT_MS);
            });
            const shutdown = this.commandInvokeLock.runExclusive(async () => {
                if (this.workerAbort.signal.aborted) {
                    throw this.workerAbort.signal.reason;
                }
                if (this.startPromise) {
                    await this.startPromise;
                    const databaseStopped = this.waitForMessage<
                        { type: "stopped"; ok: true } | { type: "stopped"; ok: false; error: string }
                    >(this.dbWorker, (data) => data?.type === "stopped").then((response) => {
                        if (!response.ok) {
                            throw new Error(response.error);
                        }
                    });
                    // Keep the DB worker alive for actor shutdown writes. Both
                    // replies are required, regardless of which arrives first.
                    await Promise.all([this.invokeWorker("stop"), databaseStopped]);
                }
            });
            this.stopPromise = Promise.race([shutdown, timeout]).catch((error) => {
                this.workerFailure = error instanceof Error ? error : new Error(String(error));
                throw this.workerFailure;
            }).finally(() => {
                clearTimeout(timer);
                // Reject in-flight commands and startup too, releasing the lock.
                // Queued commands will see workerFailure before posting anything.
                this.workerAbort.abort(this.workerFailure || new Error("Fiber stopped"));
                this.fiberWorker.terminate();
                this.dbWorker.terminate();
            });
        }
        return this.stopPromise;
    }

    private async invokeWorker(name: string, args: any[] = []): Promise<any> {
        if (this.workerFailure) {
            throw this.workerFailure;
        }
        const response = this.waitForMessage<FiberInvokeResponse>(
            this.fiberWorker, (data) => typeof data?.ok === "boolean"
        );
        this.fiberWorker.postMessage({ name, args } as FiberInvokeRequest);
        const result = await response;
        if (!result.ok) {
            throw new Error(result.error);
        }
        return result.data;
    }

    private waitForMessage<T>(worker: Worker, accepts: (data: any) => boolean): Promise<T> {
        return new Promise((resolve, reject) => {
            const signal = this.workerAbort.signal;
            const clean = () => {
                worker.removeEventListener("message", onMessage);
                signal.removeEventListener("abort", onError);
                for (const source of [this.dbWorker, this.fiberWorker]) {
                    source.removeEventListener("message", onWorkerMessage);
                    source.removeEventListener("error", onError);
                    source.removeEventListener("messageerror", onError);
                }
            };
            const onMessage = (event: MessageEvent) => {
                if (accepts(event.data)) {
                    clean();
                    resolve(event.data);
                }
            };
            const onError = () => {
                clean();
                reject(this.workerFailure || signal.reason || new Error("Fiber worker failed"));
            };
            const onWorkerMessage = (event: MessageEvent) => {
                if (event.data?.type === "error") {
                    onError();
                }
            };
            if (this.workerFailure || signal.aborted) {
                onError();
                return;
            }
            worker.addEventListener("message", onMessage);
            signal.addEventListener("abort", onError);
            for (const source of [this.dbWorker, this.fiberWorker]) {
                source.addEventListener("message", onWorkerMessage);
                source.addEventListener("error", onError);
                source.addEventListener("messageerror", onError);
            }
        });
    }
    async openChannel(params: OpenChannelParams): Promise<OpenChannelResult> {
        return await this.invokeCommand("open_channel", [params]);
    }
    /**
     * Open a channel with external funding and return the final negotiated unsigned
     * funding transaction. The caller should sign this tx once, without changing its
     * structure, then submit it with submitSignedFundingTx. When `funding_lock_script`
     * uses a custom wallet lock, pass `funding_lock_script_cell_deps` so the node can
     * build the initial unsigned transaction.
     */
    async openChannelWithExternalFunding(params: OpenChannelWithExternalFundingParams): Promise<OpenChannelWithExternalFundingResult> {
        return await this.invokeCommand("open_channel_with_external_funding", [params]);
    }
    /**
     * Submit the externally signed funding transaction. The transaction must match the
     * unsigned_funding_tx returned by openChannelWithExternalFunding, except witnesses.
     */
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
