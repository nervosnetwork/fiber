import * as wasmModule from "./pkg/fiber-wasm";
import wasm from "./pkg/fiber-wasm_bg.wasm";
wasmModule.initSync(wasm);

export default wasmModule;
