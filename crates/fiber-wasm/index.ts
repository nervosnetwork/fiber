import * as wasmModule from "./pkg/fiber-wasm";
import wasm from "./pkg/fiber-wasm_bg.wasm";
wasmModule.initSync({ module: wasm });

export default wasmModule;
