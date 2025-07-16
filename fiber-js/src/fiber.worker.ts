import { FiberInvokeRequest, FiberWorkerInitializationOptions } from "./types/general";

onerror = (err) => {
    console.error(err)
}

let fiber: any;
onmessage = async (evt) => {

    if (fiber === undefined) {
        const data = evt.data as FiberWorkerInitializationOptions;
        console.debug("Starting fiber...")
        fiber = await import("fiber-wasm");
        fiber.default.set_shared_array(data.inputBuffer, data.outputBuffer
        );
        await fiber.default.fiber(data.config, data.logLevel, data.chainSpec, data.fiberKeyPair, data.ckbSecretKey, data.databasePrefix);
        console.debug("Fiber started..")
        console.debug("fiber=", fiber);
        self.postMessage({})
        return;
    } else {
        const data = evt.data as FiberInvokeRequest;

        try {
            const result = await ((fiber.default)[data.name])(...data.args);
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
