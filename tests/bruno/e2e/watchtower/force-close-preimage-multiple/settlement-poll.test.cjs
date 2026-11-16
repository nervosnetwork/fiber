// Run with: node --test tests/bruno/e2e/watchtower/force-close-preimage-multiple/settlement-poll.test.cjs
// Execute the real Bruno hooks against a small chain/indexer model. In the CI
// trace the root is already spent while NODE1's last payout is still in the pool.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const vm = require('node:vm');

const files = [
  '22-generate-blocks-for-settlement-final-tx-committed.bru',
  '23-check-commitment-tx.bru',
  '24-check-balance-node1.bru',
  '25-check-balance-node2.bru',
];
const requests = files.map(file => {
  const source = readFileSync(join(__dirname, file), 'utf8');
  return {
    name: source.match(/name: (.*)/)[1],
    body: JSON.parse(source.match(/body:json \{\n([\s\S]*?)\n\}/)[1]),
    pre: source.match(/script:pre-request \{\n([\s\S]*?)\n\}/)?.[1] || '',
    post: source.match(/script:post-response \{\n([\s\S]*?)\n\}/)?.[1] || '',
    assertions: source.match(/assert \{\n([\s\S]*?)\n\}/)?.[1] || '',
  };
});

async function run({ confirms = true, indexerLag = 0, payout = 10090998905n,
                     max = 8, timeout = 120000, rpcError = false } = {}) {
  const baseline = 500000000000000000n;
  const vars = new Map(Object.entries({
    NODE1_BALANCE: String(baseline), NODE2_BALANCE: String(baseline),
    NODE1_FUNDING_SCRIPT: { args: 'node1' }, NODE2_FUNDING_SCRIPT: { args: 'node2' },
    TX_HASH: 'root-already-spent', settlement_poll_max: max,
    settlement_poll_timeout_ms: timeout,
  }));
  let now = 0, mines = 0, capacityReads = 0, finalAssertions = 0;
  let pool = false, committed = false, index = 0;
  const trace = [];
  while (index < requests.length) {
    assert.ok(trace.length < 100, 'request routing must be bounded');
    const request = requests[index];
    trace.push(request.name);
    let body = structuredClone(request.body), next;
    const context = vm.createContext({
      bru: {
        getVar: key => vars.get(key), setVar: (key, value) => vars.set(key, value),
        setNextRequest: name => { next = name; },
      },
      req: { getBody: () => body, setBody: value => { body = value; } },
      res: { status: 200 }, console: { log() {} },
      Date: { now: () => now },
      setTimeout: (callback, ms) => { now += ms; callback(); },
    });
    const hook = code => vm.runInContext(`(async () => {\n${code}\n})()`, context);
    await hook(request.pre);
    if (body.method === 'generate_epochs') {
      mines++;
      // The final transaction is broadcast AFTER the first mining request.
      // Further blocks must be mined for proposal/commitment to progress.
      if (pool && confirms && mines >= 3) committed = true;
      pool = true;
      context.res.body = { result: '0x250000000001' };
    } else if (body.method === 'get_live_cell') {
      context.res.body = { result: { cell: null, status: 'unknown' } };
    } else {
      assert.equal(body.method, 'get_cells_capacity');
      assert.ok(body.params[0].script, 'real pre-request must set the funding script');
      const visible = committed && capacityReads++ >= indexerLag;
      const capacity = body.params[0].script.args === 'node1'
        ? baseline - 10100000464n + (visible ? payout : 0n)
        : baseline + 8999600n; // NODE2 has already received both TLCs and its unlock.
      context.res.body = rpcError ? { error: { code: -1, message: 'indexer unavailable' } }
        : { result: { capacity: `0x${capacity.toString(16)}` } };
    }
    await hook(request.post);
    for (const line of request.assertions.trim().split('\n').filter(Boolean)) {
      const [expression, operator, value] = line.trim().match(/^(.*): (\w+) (.*)$/).slice(1);
      const actual = vm.runInContext(expression, context);
      if (operator === 'eq') assert.equal(actual, Number(value));
      else {
        finalAssertions++;
        assert.ok(operator === 'lt' ? actual < Number(value) : actual > Number(value),
          `balance assertion failed: ${actual} ${operator} ${value}; mines=${mines}`);
      }
    }
    index = next === undefined ? index + 1 : requests.findIndex(r => r.name === next);
    assert.ok(index >= 0, `unknown next request: ${next}`);
  }
  return { mines, finalAssertions, trace, vars };
}

test('spent root plus pending final payout must mine before either balance assertion', async () => {
  const result = await run();
  assert.ok(result.mines >= 3);
  assert.equal(result.finalAssertions, 2);
  assert.equal(result.vars.get('settlement_poll_iteration'), undefined);
});

test('waits for the indexer to expose the committed payout', async () => {
  const result = await run({ indexerLag: 2 });
  assert.ok(result.mines >= 5);
  assert.equal(result.finalAssertions, 2);
});

test('never-confirming final payout fails at the attempt cap with capacity diagnostics', async () => {
  await assert.rejects(run({ confirms: false, max: 3 }),
    /Final settlement.*3.*capacity=.*deficit=10100000464.*root-already-spent/);
});

test('deadline bounds polling independently of the attempt cap', async () => {
  await assert.rejects(run({ confirms: false, max: 80, timeout: 1500 }),
    /Final settlement.*elapsed=.*capacity=/);
});

test('a committed payout with an excessive fee never passes the original balance limit', async () => {
  await assert.rejects(run({ payout: 10090990000n, max: 3 }), /Final settlement/);
});

test('JSON-RPC errors fail directly instead of being treated as pending settlement', async () => {
  await assert.rejects(run({ rpcError: true }), /capacity RPC.*indexer unavailable/);
});
