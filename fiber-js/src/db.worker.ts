import { DbWorkerInitializationOptions } from "./types/general.js";

onerror = (err) => {
    console.error(err)
}
onmessage = async (evt) => {
    const data = evt.data as DbWorkerInitializationOptions;
    const wasmModule = await import("fiber-wasm-db-worker");
    wasmModule.default.set_shared_array(data.inputBuffer, data.outputBuffer);
    self.postMessage({});
    await wasmModule.default.main_loop(data.logLevel);
}


export default {} as typeof Worker & { new(): Worker };
