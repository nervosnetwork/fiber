import { DbWorkerInitializationOptions } from "./types/general.js";

onerror = (err) => {
    console.error(err)
}
self.addEventListener("unhandledrejection", (event) => {
    self.postMessage({ type: "error", error: `${event.reason}` });
});
onmessage = async (evt) => {
    try {
        const data = evt.data as DbWorkerInitializationOptions;
        const wasmModule = await import("fiber-wasm-db-worker");
        wasmModule.default.set_shared_array(data.inputBuffer, data.outputBuffer);
        self.postMessage({ type: "ready" });
        await wasmModule.default.main_loop(data.logLevel);
        self.postMessage({ type: "stopped", ok: true });
    } catch (error) {
        self.postMessage({ type: "error", error: `${error}` });
    }
}


export default {} as typeof Worker & { new(): Worker };
