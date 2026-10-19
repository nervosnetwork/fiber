// Run with `make wasm-stop-test` after installing the root and runner npm dependencies.
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import puppeteer from "puppeteer";

const root = fileURLToPath(new URL("../../", import.meta.url));
const out = await mkdtemp(join(tmpdir(), "fiber-stop-"));
const modules = {
    "fiber-wasm": [process.env.FIBER_WASM_DIR, "fiber_wasm"],
    "fiber-wasm-db-worker": [process.env.FIBER_DB_WASM_DIR, "fiber_wasm_db_worker"]
};

// Bundle the production workers with their generated WASM bindings. Embedding
// the WASM bytes avoids a separate asset server and generated snippet routes.
await build({
    absWorkingDir: root,
    entryPoints: {
        fiber: "fiber-js/src/index.ts",
        "fiber.worker": "fiber-js/src/fiber.worker.ts",
        "db.worker": "fiber-js/src/db.worker.ts"
    },
    outdir: out,
    bundle: true,
    format: "esm",
    loader: { ".yml": "text", ".wasm": "binary" },
    plugins: [{
        name: "browser-workers",
        setup(builder) {
            builder.onResolve({ filter: /\.worker\.ts$/ }, ({ path, kind }) =>
                kind === "entry-point" ? undefined : { path, namespace: "worker" });
            builder.onLoad({ filter: /.*/, namespace: "worker" }, ({ path }) => ({
                contents: `export default class extends Worker {
                    constructor() { super("/${path.includes("db.worker") ? "db" : "fiber"}.worker.js", { type: "module" }); }
                }`
            }));
            builder.onResolve({ filter: /^fiber-wasm(-db-worker)?$/ }, ({ path }) => ({ path, namespace: "wasm" }));
            builder.onLoad({ filter: /.*/, namespace: "wasm" }, ({ path }) => {
                const [directory, name] = modules[path];
                assert(directory, `Missing WASM directory for ${path}`);
                return {
                    resolveDir: resolve(directory),
                    contents: `import * as wasm from "./${name}.js";
                        import bytes from "./${name}_bg.wasm";
                        wasm.initSync({ module: bytes }); export default wasm;`
                };
            });
        }
    }]
});

const server = createServer(async ({ url }, response) => {
    response.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    response.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    if (url === "/") return response.end("<!doctype html><title>Fiber stop test</title>");
    if (!["/fiber.js", "/fiber.worker.js", "/db.worker.js"].includes(url)) return response.writeHead(404).end();
    response.setHeader("Content-Type", "text/javascript");
    response.end(await readFile(join(out, url.slice(1))));
});
let browser;
try {
    await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
    browser = await puppeteer.launch({
        headless: true,
        protocolTimeout: 30000,
        ...(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}),
        args: ["--no-sandbox"]
    });
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    const result = await page.evaluate(async () => {
        const { Fiber } = await import("/fiber.js");
        const prefix = "fiber-stop-" + crypto.randomUUID();
        const key = new Uint8Array(32).fill(1);
        const script = (name, byte) => ({ name, script: {
            code_hash: "0x" + byte.repeat(32), hash_type: "type", args: "0x"
        }, cell_deps: [] });
        // Local invoices need neither peers nor a running CKB service.
        const config = JSON.stringify({
            services: ["fiber", "ckb"],
            fiber: {
                chain: "testnet", bootnode_addrs: [], announce_listening_addr: false,
                disable_built_in_watchtower: false,
                scripts: [script("FundingLock", "11"), script("CommitmentLock", "22")]
            },
            ckb: { rpc_url: location.origin + "/ckb", udt_whitelist: [] }
        });
        const fiber = new Fiber(1024 * 1024, 1024 * 1024);
        await fiber.start(config, key, key, undefined, "error", prefix);
        const creating = fiber.newInvoice({
            amount: "0x64", currency: "Fibt", payment_preimage: "0x" + "33".repeat(32)
        });
        await fiber.stop();
        const invoice = await creating;

        const restarted = new Fiber(1024 * 1024, 1024 * 1024);
        await restarted.start(config, key, key, undefined, "error", prefix);
        const restored = await restarted.getInvoice({ payment_hash: invoice.invoice.data.payment_hash });
        await restarted.stop();
        await new Promise((resolve, reject) => {
            const request = indexedDB.deleteDatabase(prefix + "/store");
            request.onsuccess = resolve;
            request.onerror = () => reject(request.error);
            request.onblocked = () => reject(new Error("Database connection was not released"));
        });
        return { original: invoice.invoice_address, restored: restored.invoice_address };
    });
    assert.equal(result.restored, result.original);
    console.log("PASS: stop preserves pending invoice writes, restart reads them, and database connections are released");
} finally {
    await browser?.close();
    server.close();
    await rm(out, { recursive: true, force: true });
}
