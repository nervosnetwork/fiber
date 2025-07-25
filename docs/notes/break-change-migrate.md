

# Fiber Breaking Change Migration Guide

## Why

Fiber requires a breaking change migration due to significant modifications in the database schema that are incompatible with previous versions. Our first break change introduced because the new feature of basic MPP and security fix.

To upgrade to latest fiber version, a clean migration process is required to ensure data integrity and system stability.

## Prerequisites

Before starting the migration process, ensure you have:

- [ ] Fiber node is running and accessible
- [ ] Sufficient time to close all channels (can take several minutes)
- [ ] Network connectivity for possible cooperative channel closures

## Backup Old Database

Creating a backup is crucial before starting the migration process. For steps of backup, please refer to [backup-guide](https://github.com/nervosnetwork/fiber/blob/develop/docs/notes/backup-guide.md#2-backing-up-node-data)

The simplest way to create a backup is making a copy of fiber running directory:

Stop the Node, suppose `fiber` is running base on directory `fiber-dir`:
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

We plan to drop old database, but we don't need to lose any funding.

For unlokcing the funds locked in current opening channels, we need to close all channels from old database, first we try to shutdown them cooperatively(which require the peer of channel online at the moment), then if not success, we continue try to shutdown channel forcely.

### 1. List Active Channels

```bash
# Check all channels from fiber node

curl -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{}]}' | jq '.result.channels'
```

will return all channels(not include closed channels):

### 2. Cooperative Shutdown Channel

For each active channel, initiate a cooperative shutdown, please note we use `default_funding_lock_script` as the shutdown script here:


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

wait for a while to make sure shutdown transaction is submitted onto chain, and channel statuses are turned into `Closed`:

```bash
curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{ "include_closed": true }]}' | jq '.result.channels[].state'
```

will returns:

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

If all your channels are closed by cooperative shutdown, you don't need to continue to shutdown channels forcely.

### 3. Uncooperatively Shutdown Channel

If some peers of channels does not online at the moment, you may need to shutdown channels forcely.

Use this bash script to achieve it, note that we don't need to specify `shutdown_script` here:

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

Double check all the channels are `Closed`:

```bash
curl -s -X POST http://127.0.0.1:21714 \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -d '{"id": "42", "jsonrpc": "2.0", "method": "list_channels", "params": [{ "include_closed": true }]}' | jq '.result.channels[].state'
```

will return:

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

Now you have finished all the break change migration and ready to upgrade to latest fiber node.