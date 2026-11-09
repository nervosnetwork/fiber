// Run with: node --test tests/bruno/e2e/watchtower/force-close-with-pending-tlcs-and-stop-watchtower/settlement-poll.test.cjs
// Execute this scenario's real Bruno requests against the CI timeout/refund
// schedule: root absent/spent, final payout broadcast after mining, then indexed.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const vm = require('node:vm');

const files = [
  '19-generate-blocks-for-final-settlement-tx1-committed.bru',
  '20-check-commitment-tx.bru',
  '21-check-balance-node1.bru',
  '22-check-balance-node2.bru',
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

async function run({ confirms = true, indexerLag = 0, payout = 10096998905n,
                     max = 8, timeout = 120000, rpcError = false } = {}) {
  const baseline = 500000000000000000n;
  const vars = new Map(Object.entries({
    NODE1_BALANCE: String(baseline), NODE2_BALANCE: String(baseline),
    NODE1_FUNDING_SCRIPT: { args: 'node1' }, NODE2_FUNDING_SCRIPT: { args: 'node2' },
    TX_HASH: 'raw-root-never-published', settlement_poll_max: max,
    settlement_poll_timeout_ms: timeout,
  }));
  let now = 0, mines = 0, capacityReads = 0, finalAssertions = 0;
  let pool = false, committed = false, indexed = false, index = 0;
  const trace = [];
  while (index < requests.length) {
    assert.ok(trace.length < 200, 'request routing must be bounded');
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
      assert.deepEqual(body.params, ['0x1']);
      // Broadcast is after the first mining pass. Proposal/commitment progress
      // requires later mining, independently of the unknown root-cell status.
      if (pool && confirms && mines >= 3) committed = true;
      pool = true;
      context.res.body = { result: '0x300000000001' };
    } else if (body.method === 'get_live_cell') {
      context.res.body = { result: { cell: null, status: 'unknown' } };
    } else {
      assert.equal(body.method, 'get_cells_capacity');
      assert.ok(body.params[0].script, 'real pre-request must set the funding script');
      if (committed && capacityReads++ >= indexerLag) indexed = true;
      const capacity = body.params[0].script.args === 'node1'
        ? baseline - 10097001443n + (indexed ? payout : 0n)
        : baseline - 10000001000n;
      context.res.body = rpcError ? { error: { code: -1, message: 'indexer unavailable' } }
        : { result: { capacity: `0x${capacity.toString(16)}` } };
    }
    await hook(request.post);
    for (const line of request.assertions.trim().split('\n').filter(Boolean)) {
      const [expression, operator, value] = line.trim().match(/^(.*): (\w+) (.*)$/).slice(1);
      const actual = vm.runInContext(expression, context);
      if (operator === 'eq') assert.equal(actual, JSON.parse(value));
      else {
        // Evaluate the unchanged balance limit first to reproduce CI's exact
        // deficit; also require actual chain/indexer progress before assertions.
        assert.ok(actual < Number(value),
          `balance assertion failed: ${actual} ${operator} ${value}; mines=${mines}`);
        assert.ok(indexed, 'final balance assertions must wait for the indexed payout');
        finalAssertions++;
      }
    }
    index = next === undefined ? index + 1 : requests.findIndex(r => r.name === next);
    assert.ok(index >= 0, `unknown next request in this scenario: ${next}`);
  }
  return { mines, finalAssertions, vars };
}

test('unknown root cannot finish a refund while the final payout is pending', async () => {
  const result = await run();
  assert.ok(result.mines >= 3);
  assert.equal(result.finalAssertions, 2);
  assert.equal(result.vars.get('settlement_poll_iteration'), undefined);
  assert.equal(result.vars.get('settlement_poll_started_at'), undefined);
});

test('waits for the indexer to expose the committed refund', async () => {
  const result = await run({ indexerLag: 2 });
  assert.ok(result.mines >= 5);
  assert.equal(result.finalAssertions, 2);
});

test('never-confirming refund fails at the attempt cap with actual capacity diagnostics', async () => {
  await assert.rejects(run({ confirms: false, max: 3 }),
    /Final settlement.*3.*capacity=.*deficit=10097001443.*raw-root-never-published/);
});

test('deadline bounds polling independently of the attempt cap', async () => {
  await assert.rejects(run({ confirms: false, max: 80, timeout: 1500 }),
    /Final settlement.*elapsed=.*capacity=/);
});

test('a committed refund with a fee above the original limit never passes', async () => {
  await assert.rejects(run({ payout: 10096995000n, max: 3 }), /Final settlement/);
});

test('the original 5000 fee boundary remains strictly excluded', async () => {
  await assert.rejects(run({ payout: 10097001443n - 5000n, max: 3 }), /Final settlement/);
});

test('an over-refund cannot satisfy the nonnegative deficit predicate', async () => {
  await assert.rejects(run({ payout: 10097001444n, max: 3 }), /Final settlement/);
});

test('JSON-RPC errors fail directly instead of being treated as pending refunds', async () => {
  await assert.rejects(run({ rpcError: true }), /capacity RPC.*indexer unavailable/);
});
