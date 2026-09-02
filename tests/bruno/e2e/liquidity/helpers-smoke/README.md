# Liquidity Helper Smoke Tests

Run the Bruno helper smoke suite with its local JSON-RPC responders:

```bash
tests/bruno/e2e/liquidity/helpers-smoke/run.sh
```

The runner starts the responders and executes this command from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/helpers-smoke -r --env test
```

Run the shell artifact-writer security and redaction regression directly:

```bash
tests/bruno/scripts/test-write-liquidity-diagnostics.sh
```

The smoke runner uses only Node built-ins and Bruno-provided axios. It starts
temporary responders on ports 8114, 21714, and 21715; port 1 remains closed for
the bounded transient-error check.
