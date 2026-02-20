import { FiberInvokeRequest, FiberWorkerInitializationOptions } from "./types/general";

/** Resolve WASM export by name. wasm-bindgen may export api::fn as "fn" or "api_fn". */
function resolveWasmFunction(wasm: Record<string, unknown>, name: string): ((...args: unknown[]) => unknown) | undefined {
    const direct = wasm[name];
    if (typeof direct === "function") return direct as (...args: unknown[]) => unknown;
    const prefixed = wasm[`api_${name}`];
    if (typeof prefixed === "function") return prefixed as (...args: unknown[]) => unknown;
    return undefined;
}

onerror = (err) => {
    console.error(err)
}

let fiber: any;
onmessage = async (evt) => {

    if (fiber === undefined) {
        const data = evt.data as FiberWorkerInitializationOptions;
        console.debug("Starting fiber, configuration: ", data);
        fiber = await import("fiber-wasm");
        fiber.default.set_shared_array(data.inputBuffer, data.outputBuffer
        );
        await fiber.default.fiber(
            data.config,
            data.logLevel,
            data.chainSpec,
            data.fiberKeyPair,
            data.ckbSecretKey,
            data.databasePrefix);
        console.debug("Fiber started..")
        self.postMessage({})
        return;
    } else {
        const data = evt.data as FiberInvokeRequest;

        const fn = resolveWasmFunction(fiber.default, data.name);
        if (typeof fn !== "function") {
            const err = `fiber-wasm does not export "${data.name}". ` +
                `This may mean fiber-wasm needs to be rebuilt. ` +
                `Run: cd crates/fiber-wasm && wasm-pack build --target web`;
            console.error(err);
            self.postMessage({ ok: false, error: err });
            return;
        }

        try {
            const result = await fn(...data.args);
            self.postMessage({
                ok: true,
                data: result
            })
        } catch (e) {
            self.postMessage({
                ok: false,
                error: `${e}`
            })
            console.error(e);
        }
    }

}

export default {} as typeof Worker & { new(): Worker };
