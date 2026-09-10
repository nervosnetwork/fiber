const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const { test } = require("node:test");

function fixture({ matureAfter = 0, missingMedian = false, wallDelay = 0 } = {}) {
  let epochs = 0;
  let now = (2000 - wallDelay) * 1000;
  const axios = { post: async (_, request) => {
    let result;
    if (request.method === "get_tip_header") {
      result = { hash: "0xtip", timestamp: "0x1e8480" };
    } else if (request.method === "get_block_median_time") {
      result = missingMedian ? null : `0x${(epochs >= matureAfter ? 2000000 : 1999999).toString(16)}`;
    } else if (request.method === "generate_epochs") {
      assert.deepEqual(Array.from(request.params), ["0x1"]);
      assert.ok(now >= 2000000, "do not spend the mining budget before wall-clock maturity");
      epochs++;
      result = "0xa0000000001";
    } else {
      throw new Error(`unexpected method ${request.method}`);
    }
    return { data: { jsonrpc: "2.0", id: request.id, result } };
  } };
  function load(name) {
    const context = {
      module: { exports: {} }, process,
      Date: { now: () => now },
      setTimeout: (resolve, ms) => { now += ms; resolve(); },
      require: (id) => id === "axios" ? axios : load("wait-liquidity-state"),
    };
    vm.runInNewContext(fs.readFileSync(path.join(__dirname, `${name}.js`), "utf8"), context);
    return context.module.exports;
  }
  return { helper: load("refund-tx"), epochs: () => epochs };
}

test("refund maturity unwraps RPC results and returns seconds", async () => {
  const { helper } = fixture();
  const result = await helper.waitRefundMaturity({ ckbRpcUrl: "fixture", maturitySeconds: 2000 });
  assert.equal(BigInt(result.medianSeconds), BigInt(2000));
  assert.equal(result.generatedEpochs, 0);
});

test("refund maturity does not confuse milliseconds with seconds or blocks with epochs", async () => {
  const { helper, epochs } = fixture({ matureAfter: 4 });
  const result = await helper.waitRefundMaturity({ ckbRpcUrl: "fixture", maturitySeconds: 2000 });
  assert.equal(epochs(), 4);
  assert.equal(result.generatedEpochs, 4);
});

test("refund maturity waits for wall time before consuming its epoch budget", async () => {
  const { helper, epochs } = fixture({ matureAfter: 4, wallDelay: 50 });
  await helper.waitRefundMaturity({ ckbRpcUrl: "fixture", maturitySeconds: 2000 });
  assert.equal(epochs(), 4);
});

test("refund maturity rejects a missing median", async () => {
  const { helper } = fixture({ missingMedian: true });
  await assert.rejects(helper.waitRefundMaturity({ ckbRpcUrl: "fixture", maturitySeconds: 2000 }), /returned no median/);
});

test("refund maturity bounds generated epochs", async () => {
  const { helper, epochs } = fixture({ matureAfter: Infinity });
  await assert.rejects(helper.waitRefundMaturity({ ckbRpcUrl: "fixture", maturitySeconds: 2000 }), /after generating 12 epochs/);
  assert.equal(epochs(), 12);
});
