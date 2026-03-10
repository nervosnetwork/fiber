# fnn-cli

Command-line interface for [Fiber Network Node (FNN)](https://github.com/nervosnetwork/fiber). Supports both one-shot command execution and an interactive REPL with tab completion.

## Installation

```bash
cargo build -p fnn-cli --release
# binary is at target/release/fnn-cli
```

## Quick Start

```bash
# Interactive mode (connects to local node by default)
fnn-cli

# One-shot command
fnn-cli info

# Connect to a remote node
fnn-cli -u http://54.178.252.1:8227 info

# Connect with authentication
fnn-cli -u http://54.178.252.1:8227 --auth-token 'YOUR_TOKEN' info
```

## Global Options

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--url` | `-u` | `http://127.0.0.1:8227` | RPC endpoint URL |
| `--auth-token` | | | Bearer token for RPC authentication |
| `--auth-token-file` | | | Read bearer token from a file |
| `--output-format` | `-o` | `yaml` | Output format: `json` or `yaml` |
| `--raw-data` | | | Output raw JSON-RPC response data |
| `--color` | | `auto` | Color output: `auto`, `always`, or `never` |
| `--no-banner` | | | Suppress the banner in interactive mode |

The auth token can also be set via the `FNN_AUTH_TOKEN` environment variable.

## Authentication

Deployed Fiber nodes may require bearer token (Biscuit) authentication. There are three ways to provide the token:

```bash
# 1. CLI flag (token must be on a single line, no whitespace)
fnn-cli --auth-token 'EsQCCtkBCghjaGFu...' info

# 2. Read from file (recommended for long tokens)
echo -n 'EsQCCtkBCghjaGFu...' > ~/.fnn-token
fnn-cli --auth-token-file ~/.fnn-token info

# 3. Environment variable
export FNN_AUTH_TOKEN='EsQCCtkBCghjaGFu...'
fnn-cli info
```

> **Note:** Tokens are often long base64 strings. When passing via `--auth-token`, make sure the token is on a single line with no embedded whitespace. Using `--auth-token-file` or `FNN_AUTH_TOKEN` avoids shell quoting issues.

## Interactive Mode

Run `fnn-cli` without a subcommand to enter the interactive REPL:

```
$ fnn-cli -u http://54.178.252.1:8227

  ______ ___ ___  ______ _____
 |  ____|_ _| _ \|  ____|  __ \
 | |__   | ||___/| |__  | |__) |
 |  __|  | || _ \|  __| |  _  /
 | |    _| || |_\| |____| | \ \
 |_|   |___|____/|______|_|  \_\

[  fnn-cli version ]: 0.7.1
[              url ]: http://54.178.252.1:8227
[    output format ]: yaml
[           status ]: Connected

FNN> info
FNN> channel list_channels
FNN> exit
```

Features:
- **Tab completion** for commands, subcommands, and `--flags`
- **Command history** persisted to `~/.fnn_cli_history`
- **Colored output** with syntax highlighting for JSON/YAML responses
- **Shell-like quoting** — supports single quotes, double quotes, and backslash escaping

## Commands

### info

Get node information.

```bash
fnn-cli info
```

### peer — Manage peer connections

| Subcommand | Description |
|---|---|
| `connect_peer` | Connect to a peer |
| `disconnect_peer` | Disconnect from a peer |
| `list_peers` | List connected peers |

```bash
fnn-cli peer connect_peer --address /ip4/1.2.3.4/tcp/8119/p2p/QmNode...
fnn-cli peer disconnect_peer --pubkey 02abc...
fnn-cli peer list_peers
```

### channel — Manage payment channels

| Subcommand | Description |
|---|---|
| `open_channel` | Open a channel with a peer |
| `accept_channel` | Accept a channel opening request |
| `list_channels` | List all channels |
| `update_channel` | Update channel parameters |
| `shutdown_channel` | Shut down a channel |
| `abandon_channel` | Abandon a channel |

```bash
fnn-cli channel open_channel --pubkey 02abc... --funding-amount 1000000
fnn-cli channel list_channels
fnn-cli channel shutdown_channel --channel-id 0xabc...
```

### invoice — Manage invoices

| Subcommand | Description |
|---|---|
| `new_invoice` | Generate a new invoice |
| `parse_invoice` | Parse an encoded invoice string |
| `get_invoice` | Get an invoice by payment hash |
| `cancel_invoice` | Cancel an invoice |
| `settle_invoice` | Settle an invoice with a preimage |

```bash
fnn-cli invoice new_invoice --amount 1000 --currency fibb --description "test payment"
fnn-cli invoice parse_invoice --invoice fibb1...
fnn-cli invoice get_invoice --payment-hash 0xabc...
```

### payment — Manage payments

| Subcommand | Description |
|---|---|
| `send_payment` | Send a payment |
| `get_payment` | Get a payment by hash |
| `list_payments` | List all payments |
| `build_router` | Build a payment route |
| `send_payment_with_router` | Send a payment with a manually built route |

```bash
fnn-cli payment send_payment --invoice fibb1...
fnn-cli payment get_payment --payment-hash 0xabc...
fnn-cli payment list_payments
```

### graph — Query the network graph

| Subcommand | Description |
|---|---|
| `graph_nodes` | List nodes in the network graph |
| `graph_channels` | List channels in the network graph |

```bash
fnn-cli graph graph_nodes
fnn-cli graph graph_channels --limit 10
```

### cch — Cross-chain hub operations

| Subcommand | Description |
|---|---|
| `send_btc` | Create a CCH order to pay a BTC Lightning invoice |
| `receive_btc` | Create a CCH order to receive BTC via Fiber |
| `get_cch_order` | Get a CCH order by payment hash |

```bash
fnn-cli cch send_btc --btc-pay-req lnbc1...
fnn-cli cch get_cch_order --payment-hash 0xabc...
```

### watchtower — Watchtower service

| Subcommand | Description |
|---|---|
| `create_watch_channel` | Create a new watched channel |
| `remove_watch_channel` | Remove a watched channel |
| `update_revocation` | Update revocation data |
| `update_pending_remote_settlement` | Update pending remote settlement data |
| `update_local_settlement` | Update local settlement data |
| `create_preimage` | Store a preimage |
| `remove_preimage` | Remove a stored preimage |

### dev — Development/debug commands

> **Warning:** These commands are for development and debugging only. Do not use in production.

| Subcommand | Description |
|---|---|
| `commitment_signed` | Send a commitment_signed message |
| `add_tlc` | Add a TLC to a channel |
| `remove_tlc` | Remove a TLC from a channel |
| `submit_commitment_transaction` | Submit a commitment transaction on-chain |
| `check_channel_shutdown` | Trigger CheckShutdownTx on a channel |

### prof — Profiling

| Subcommand | Description |
|---|---|
| `pprof` | Collect a CPU profile and generate a flamegraph SVG |

```bash
fnn-cli prof pprof --duration-secs 30
```

## Getting Help

Every command and subcommand supports `--help`:

```bash
fnn-cli --help
fnn-cli channel --help
fnn-cli channel open_channel --help
```

In interactive mode, append `--help` to any command:

```
FNN> channel open_channel --help
```

## Output Formats

```bash
# YAML output (default, human-friendly)
fnn-cli info

# JSON output
fnn-cli -o json info

# Raw JSON-RPC response (for scripting)
fnn-cli --raw-data info
```

## Tips

- Use `--raw-data` combined with tools like `jq` for scripting:
  ```bash
  fnn-cli --raw-data -o json info | jq '.node_name'
  ```

- JSON arguments (e.g., UDT type scripts) should be passed as quoted strings:
  ```bash
  fnn-cli channel open_channel --pubkey 02abc... \
    --funding-amount 1000000 \
    --funding-udt-type-script '{"code_hash":"0x...", "hash_type":"type", "args":"0x..."}'
  ```

- In interactive mode, use **Tab** to auto-complete commands and flags.
