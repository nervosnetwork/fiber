import * as wasmModule from "./pkg/fiber-wasm-db-worker";
import wasm from "./pkg/fiber-wasm-db-worker_bg.wasm"
wasmModule.initSync(wasm);

export default wasmModule;
