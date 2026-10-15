import { afterEach, beforeEach, expect, jest, test } from "@jest/globals";

class MockWorker extends EventTarget {
    postMessage = jest.fn<(data: any) => void>();
    terminate = jest.fn();

    reply(data: any) {
        this.dispatchEvent(new MessageEvent("message", { data }));
    }

    fail(message: string) {
        this.dispatchEvent(Object.assign(new Event("error"), { message }));
    }
}

jest.mock("../src/db.worker.ts", () => ({
    __esModule: true,
    default: jest.fn(() => new MockWorker())
}));
jest.mock("../src/fiber.worker.ts", () => ({
    __esModule: true,
    default: jest.fn(() => new MockWorker())
}));

const { Fiber } = require("../src/index.ts") as typeof import("../src/index.ts");
const DbWorker = require("../src/db.worker.ts").default as ReturnType<typeof jest.fn>;
const FiberWorker = require("../src/fiber.worker.ts").default as ReturnType<typeof jest.fn>;

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

beforeEach(() => {
    jest.clearAllMocks();
});
afterEach(() => {
    jest.useRealTimers();
});

function createFiber() {
    const fiber = new Fiber(1024, 1024);
    const dbWorker = DbWorker.mock.results.at(-1)!.value as MockWorker;
    const fiberWorker = FiberWorker.mock.results.at(-1)!.value as MockWorker;
    return { fiber, dbWorker, fiberWorker };
}

async function startFiber() {
    const instance = createFiber();
    const started = instance.fiber.start("", new Uint8Array(32));
    instance.dbWorker.reply({ type: "ready" });
    instance.fiberWorker.reply({ type: "ready" });
    await started;
    return instance;
}

test.each(["fiber", "database"])("stop waits for both replies and is idempotent (%s replies first)", async (first) => {
    const { fiber, dbWorker, fiberWorker } = await startFiber();
    const stopped = jest.fn();
    const shutdown = fiber.stop();
    expect(fiber.stop()).toBe(shutdown);
    const stopping = shutdown.then(stopped);
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenLastCalledWith({ name: "stop", args: [] });

    const replyFiber = () => fiberWorker.reply({ ok: true });
    const replyDatabase = () => dbWorker.reply({ type: "stopped", ok: true });
    (first === "fiber" ? replyFiber : replyDatabase)();
    await tick();
    expect(stopped).not.toHaveBeenCalled();
    expect(dbWorker.terminate).not.toHaveBeenCalled();
    expect(fiberWorker.terminate).not.toHaveBeenCalled();

    (first === "fiber" ? replyDatabase : replyFiber)();
    await stopping;
    await fiber.stop();
    expect(dbWorker.terminate).toHaveBeenCalledTimes(1);
    expect(fiberWorker.terminate).toHaveBeenCalledTimes(1);
});

test("stop drains accepted calls and rejects new calls immediately", async () => {
    const { fiber, dbWorker, fiberWorker } = await startFiber();
    const first = fiber.invokeCommand("first_write");
    const second = fiber.invokeCommand("second_write");
    const stopping = fiber.stop();
    await expect(fiber.nodeInfo()).rejects.toThrow("stopping or stopped");
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenLastCalledWith({ name: "first_write", args: [] });
    fiberWorker.reply({ ok: true });
    await first;
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenLastCalledWith({ name: "second_write", args: [] });
    fiberWorker.reply({ ok: true });
    await second;
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenLastCalledWith({ name: "stop", args: [] });
    fiberWorker.reply({ ok: true });
    dbWorker.reply({ type: "stopped", ok: true });
    await stopping;
    await expect(fiber.nodeInfo()).rejects.toThrow("stopping or stopped");
});

test("stop during startup waits for initialization", async () => {
    const { fiber, dbWorker, fiberWorker } = createFiber();
    const started = fiber.start("", new Uint8Array(32));
    const stopping = fiber.stop();
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenCalledTimes(1);
    fiberWorker.reply({ type: "ready" });
    dbWorker.reply({ type: "ready" });
    await started;
    await tick();
    expect(fiberWorker.postMessage).toHaveBeenLastCalledWith({ name: "stop", args: [] });
    dbWorker.reply({ type: "stopped", ok: true });
    fiberWorker.reply({ ok: true });
    await stopping;
});

test("stop before start releases workers and prevents startup", async () => {
    const { fiber, dbWorker, fiberWorker } = createFiber();
    await fiber.stop();
    expect(dbWorker.terminate).toHaveBeenCalledTimes(1);
    expect(fiberWorker.postMessage).not.toHaveBeenCalled();
    await expect(fiber.start("", new Uint8Array(32))).rejects.toThrow("already been started or stopped");
});

test("stop during failed startup rejects and releases both workers", async () => {
    const { fiber, dbWorker, fiberWorker } = createFiber();
    const started = fiber.start("", new Uint8Array(32));
    const startRejected = expect(started).rejects.toThrow("startup failed");
    const stopping = fiber.stop();
    const stopRejected = expect(stopping).rejects.toThrow("startup failed");
    dbWorker.reply({ type: "error", error: "startup failed" });
    await Promise.all([startRejected, stopRejected]);
    expect(dbWorker.terminate).toHaveBeenCalledTimes(1);
    expect(fiberWorker.terminate).toHaveBeenCalledTimes(1);
});

test.each(["state check", "database", "worker"])("stop rejects and terminates both workers on %s failure", async (failure) => {
    const { fiber, dbWorker, fiberWorker } = await startFiber();
    const reason = failure === "state check" ? "Fiber panicked, please refresh page" : "shutdown failed";
    const stopping = fiber.stop();
    const rejected = expect(stopping).rejects.toThrow(reason);
    await tick();
    if (failure === "state check") {
        fiberWorker.reply({ ok: false, error: reason });
    } else if (failure === "database") {
        dbWorker.reply({ type: "error", error: reason });
    } else {
        dbWorker.fail(reason);
    }
    await rejected;
    expect(dbWorker.terminate).toHaveBeenCalledTimes(1);
    expect(fiberWorker.terminate).toHaveBeenCalledTimes(1);
    await expect(fiber.stop()).rejects.toThrow(reason);
});

test.each(["startup", "command", "fiber", "database"])("stop times out while waiting for %s and rejects pending calls", async (waitingFor) => {
    const { fiber, dbWorker, fiberWorker } = createFiber();
    const started = fiber.start("", new Uint8Array(32));
    const pending: Promise<unknown>[] = [];
    if (waitingFor === "startup") {
        pending.push(expect(started).rejects.toThrow("stop timed out"));
    } else {
        dbWorker.reply({ type: "ready" });
        fiberWorker.reply({ type: "ready" });
        await started;
    }
    if (waitingFor === "command") {
        for (const name of ["stuck", "queued"]) {
            pending.push(expect(fiber.invokeCommand(name)).rejects.toThrow("stop timed out"));
        }
    }
    jest.useFakeTimers({ doNotFake: ["setImmediate"] });
    const stopping = fiber.stop();
    const rejected = expect(stopping).rejects.toThrow("stop timed out");
    await tick();
    if (waitingFor === "fiber") dbWorker.reply({ type: "stopped", ok: true });
    if (waitingFor === "database") fiberWorker.reply({ ok: true });

    await jest.advanceTimersByTimeAsync(29_999);
    expect(dbWorker.terminate).not.toHaveBeenCalled();
    expect(fiberWorker.terminate).not.toHaveBeenCalled();
    await jest.advanceTimersByTimeAsync(1);
    await Promise.all([rejected, ...pending]);
    await tick();
    expect(dbWorker.terminate).toHaveBeenCalledTimes(1);
    expect(fiberWorker.terminate).toHaveBeenCalledTimes(1);
    expect(jest.getTimerCount()).toBe(0);
    const commands = fiberWorker.postMessage.mock.calls.map(([data]) => data.name).filter(Boolean);
    expect(commands).toEqual(waitingFor === "startup" ? [] : waitingFor === "command" ? ["stuck"] : ["stop"]);
    await expect(fiber.stop()).rejects.toThrow("stop timed out");
    await expect(fiber.nodeInfo()).rejects.toThrow("stopping or stopped");
});

test("a DB worker crash rejects an in-flight API and subsequent stop", async () => {
    const { fiber, dbWorker, fiberWorker } = await startFiber();
    const result = fiber.nodeInfo();
    const rejected = expect(result).rejects.toThrow("database crashed");
    await tick();
    dbWorker.fail("database crashed");
    await rejected;
    await expect(fiber.stop()).rejects.toThrow("database crashed");
    expect(fiberWorker.terminate).toHaveBeenCalledTimes(1);
});
