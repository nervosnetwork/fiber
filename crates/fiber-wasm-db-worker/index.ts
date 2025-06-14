import * as wasmModule from "./pkg/fiber-wasm-db-worker";
import wasm from "./pkg/fiber-wasm-db-worker_bg.wasm"
wasmModule.initSync({ module: wasm });

export default wasmModule;
