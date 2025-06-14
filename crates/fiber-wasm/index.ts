import * as wasmModule from "./pkg/fiber-wasm";
import wasm from "./pkg/fiber-wasm_bg.wasm";
await wasmModule.default(wasm);

export default wasmModule;
