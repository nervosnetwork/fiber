

# Fiber Breaking Change Migration Guide

## Why

Fiber requires a breaking change migration due to significant modifications in the database schema that are incompatible with previous versions. Our first breaking change was introduced because of the new basic MPP feature and security fixes.

To upgrade to the latest Fiber version, a clean migration process is required to ensure data integrity and system stability.

## Prerequisites

Before starting the migration process, ensure you have:

- [ ] Fiber node is running and accessible
- [ ] Sufficient time to close all channels (can take several minutes)
- [ ] Network connectivity for possible cooperative channel closures

## Backup Old Database

Creating a backup is crucial before starting the migration process. For backup steps, please refer to the [backup-guide](https://github.com/nervosnetwork/fiber/blob/develop/docs/notes/backup-guide.md#2-backing-up-node-data).

The simplest way to create a backup is to make a copy of the Fiber running directory:

Stop the Node. Suppose `fiber` is running based on directory `fiber-dir`:
```bash
# Check for running node processes
ps aux | grep fnn
# Example output: ckb 3585187 0.7 2.4 756932 194052 ? Sl May07 71:52 ./fnn -c ./fiber-dir/config.yml -d ./fiber-dir
# Terminate it (replace 3585187 with your process ID)
kill 3585187
```

Create Backup:
```bash
tar -zcvf backup-fiber-dir.tar.gz fiber-dir
```

## Close All Channels

We plan to drop the old database, but we don't want to lose any funds.

To unlock the funds locked in currently open channels, we need to close all channels from the old database. First, we try to shut them down cooperatively (which requires the channel peer to be online at the moment), then if that is not successful, we continue to try to shut down channels forcefully.

### 1. List Active Channels

```bash
# Check all channels from the Fiber node

curl -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{}]}' | jq '.result.channels'
```

This will return all channels (not including closed channels):

### 2. Cooperative Shutdown Channel

For each active channel, initiate a cooperative shutdown. Please note we use `default_funding_lock_script` as the shutdown script here:


```bash
#!/bin/bash
default_funding_lock_script=$(curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "node_info", "params": [{}]}' | jq -r '.result.default_funding_lock_script')

code_hash=$(echo "$default_funding_lock_script" | jq -r '.code_hash')
hash_type=$(echo "$default_funding_lock_script" | jq -r '.hash_type')
script_args=$(echo "$default_funding_lock_script" | jq -r '.args')


# First, get all channel IDs as a string
channel_ids=$(curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{}]}' \
  | jq -r '.result.channels[].channel_id')

IFS=$'\n'
channel_array=($channel_ids)
unset IFS

for channel_id in "${channel_array[@]}"; do
  echo "Shutting down channel: $channel_id"

  # Send shutdown request for each channel
  curl -X POST http://127.0.0.1:21714 \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "{\"id\": \"42\", \"jsonrpc\": \"2.0\", \"method\": \"shutdown_channel\", \"params\": [{\"channel_id\": \"$channel_id\", \"close_script\": { \"code_hash\": \"$code_hash\", \"hash_type\": \"$hash_type\", \"args\": \"$script_args\" }, \"fee_rate\": \"0x3E8\", \"force\": false}] }" | jq .

  echo "-------"
done
```

Wait for a while to make sure the shutdown transaction is submitted to the chain, and channel statuses are changed to `Closed`:

```bash
curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{ "include_closed": true }]}' | jq '.result.channels[].state'
```

This will return:

```json
{
  "state_name": "CLOSED",
  "state_flags": "COOPERATIVE"
}
{
  "state_name": "CLOSED",
  "state_flags": "COOPERATIVE"
}
```

If all your channels are closed by cooperative shutdown, you don't need to continue to shut down channels forcefully.

### 3. Uncooperative Shutdown Channel

If some channel peers are not online at the moment, you may need to shut down channels forcefully.

Use this bash script to achieve it. Note that we don't need to specify `close_script` here:

```bash
#!/bin/bash
# First, get all channel IDs as a string
channel_ids=$(curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{}]}' \
  | jq -r '.result.channels[].channel_id')

IFS=$'\n'
channel_array=($channel_ids)
unset IFS

for channel_id in "${channel_array[@]}"; do
  echo "Shutting down channel: $channel_id"

  # Send shutdown request for each channel
  curl -X POST http://127.0.0.1:21714 \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "{\"id\": \"42\", \"jsonrpc\": \"2.0\", \"method\": \"shutdown_channel\", \"params\": [{\"channel_id\": \"$channel_id\", \"force\": true}] }" | jq .

  echo "-------"
done
```

Double-check that all channels are `Closed`:

```bash
curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{ "include_closed": true }]}' | jq '.result.channels[].state'
```

This will return:

```json
{
  "state_name": "CLOSED",
  "state_flags": "UNCOOPERATIVE"
}
{
  "state_name": "CLOSED",
  "state_flags": "UNCOOPERATIVE"
}
```

Now you have finished the breaking change migration and are ready to upgrade to the latest Fiber node.