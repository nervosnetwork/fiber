# Fiber Network Node RPC

The RPC module provides a set of APIs for developers to interact with FNN. Please note that APIs are not stable yet and may change in the future.

Allowing arbitrary machines to access the JSON-RPC port (using the `rpc.listening_addr` configuration option) is **dangerous and strongly discouraged**. Please strictly limit the access to only trusted machines.

You may refer to the e2e test cases in the `tests/bruno/e2e` directory for examples of how to use the RPC.

## Table of Contents

* [RPC Methods](#rpc-methods)

    * [Module Cross Chain Hub](#module-cch)
        * [Method `send_btc`](#send_btc)

    * [Module Channel](#module-channel)
        * [Method `open_channel`](#open_channel)
        * [Method `accept_channel`](#accept_channel)
        * [Method `list_channels`](#list_channels)
        * [Method `add_tlc`](#add_tlc)
        * [Method `remove_tlc`](#remove_tlc)
        * [Method `shutdown_channel`](#shutdown_channel)
        * [Method `send_payment`](#send_payment)
        * [Method `get_payment`](#get_payment)

    * [Module Invoice](#module-invoice)
        * [Method `new_invoice`](#new_invoice)
        * [Method `parse_invoice`](#parse_invoice)

    * [Module Peer](#module-peer)
        * [Method `connect_peer`](#connect_peer)
        * [Method `disconnect_peer`](#disconnect_peer)

    * [Module Graph](#module-graph)
        * [Method `graph_nodes`](#graph_nodes)
        * [Method `graph_channels`](#graph_channels)

    * [Module Info](#module-info)
        * [Method `node_info`](#node_info)

## RPC Modules

### Module `Cch`

RPC module for cross chain hub demonstration.

<a id="send_btc"></a>
#### Method `send_btc`

###### Params

* `btc_pay_req` - Bitcoin payment request string

###### Returns

Returns null when the payment request string is valid. Otherwise, returns an error message.

### Module `Channel`

RPC module for channel management.

<a id="open_channel"></a>
#### Method `open_channel`

Attempts to open a channel with a peer.

###### Params

* `peer_id` - The peer ID to open a channel with
* `funding_amount` - The amount of CKB or UDT to fund the channel with
* `public` - Whether this is a public channel (will be broadcasted to network, and can be used to forward TLCs), an optional parameter (default value false)
* `funding_udt_type_script` - The type script of the UDT to fund the channel with, an optional parameter
* `shutdown_script` - The script used to receive the channel balance, an optional parameter, default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key
* `commitment_fee_rate` - The fee rate for the commitment transaction, an optional parameter
* `funding_fee_rate` - The fee rate for the funding transaction, an optional parameter
* `tlc_locktime_expiry_delta` - The expiry delta for the TLC locktime, an optional parameter
* `tlc_min_value` - The minimum value for a TLC, an optional parameter
* `tlc_max_value` - The maximum value for a TLC, an optional parameter
* `tlc_fee_proportional_millionths` - The fee proportional millionths for a TLC, an optional parameter
* `max_tlc_value_in_flight` - The maximum value in flight for TLCs, an optional parameter
* `max_tlc_number_in_flight` - The maximum number of TLCs that can be accepted, an optional parameter

###### Returns

* `temporary_channel_id` - The temporary channel ID of the channel being opened

<a id="accept_channel"></a>
#### Method `accept_channel`

Accepts a channel opening request from a peer.

###### Params

* `temporary_channel_id` - The temporary channel ID of the channel to accept
* `funding_amount` - The amount of CKB or UDT to fund the channel with
* `shutdown_script` - The script used to receive the channel balance, an optional parameter, default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key

###### Returns

* `channel_id` - The final ID of the channel that was accepted, it's different from the temporary channel ID

<a id="list_channels"></a>
#### Method `list_channels`

Lists all active channels that the node is participating in.

###### Params

* `peer_id` - Only list channels with this remote peer ID, an optional parameter

###### Returns

* `channels` - An array of channel objects
    * `channel_id` - The ID of the channel
    * `peer_id` - The remote peer ID of the channel
    * `funding_udt_type_script` - The type script of the UDT used to fund the channel, may be null
    * `status` - The status of the channel
    * `local_balance` - The balance of the channel owned by the local node
    * `remote_balance` - The balance of the channel owned by the remote peer
    * `offered_tlc_balance` - The total balance of currently offered TLCs in the channel
    * `received_tlc_balance` - The total balance of currently received TLCs in the channel
    * `created_at` - The timestamp when the channel was created, in milliseconds

<a id="add_tlc"></a>
#### Method `add_tlc`

Adds a TLC to the channel.

###### Params

* `channel_id` - The ID of the channel to add the TLC to
* `amount` - The amount of CKB or UDT to add to the TLC
* `payment_hash` - The payment hash of the TLC
* `expiry` - The expiry time of the TLC

###### Returns

* `tlc_id` - The ID of the TLC that was added

<a id="remove_tlc"></a>
#### Method `remove_tlc`

Removes a TLC from the channel.

###### Params

* `channel_id` - The ID of the channel to remove the TLC from
* `tlc_id` - The ID of the TLC to remove
* `reason` - The reason for removing the TLC, either a 32-byte hash for preimage fulfillment or an u32 error code for removal

###### Returns

Returns null when the TLC is removed successfully. Otherwise, returns an error message.

<a id="shutdown_channel"></a>
#### Method `shutdown_channel`

Attempts to close the channel mutually.

###### Params

* `channel_id` - The ID of the channel to close
* `close_script` - The script used to receive the channel balance, only support secp256k1_blake160_sighash_all script for now
* `fee_rate` - The fee rate for the closing transaction, the fee will be deducted from the closing initiator's channel balance

###### Returns

Returns null when the request is successful. Otherwise, returns an error message.

<a id="send_payment"></a>
#### Method `send_payment`

Sends a payment to a peer.

###### Params

- `target_pubkey` (type: `Pubkey`): The identifier of the payment target.
- `amount` (type: `u128`): The amount of the payment.
- `payment_hash` (type: `Hash256`): The hash to use within the payment's HTLC.
- `final_cltv_delta` (type: `Option<u64>`): The CLTV delta from the current height that should be used to set the timelock for the final hop.
- `invoice` (type: `Option<String>`): The encoded invoice to send to the recipient.
- `timeout` (type: `Option<u64>`): The payment timeout in seconds. If the payment is not completed within this time, it will be cancelled.
- `max_fee_amount` (type: `Option<u128>`): The maximum fee amounts in shannons that the sender is willing to pay.
- `max_parts` (type: `Option<u64>`): Max parts for the payment, only used for multi-part payments.

Note `target_pubkey`, `amount`, `payment_hash` should be consistent with the invoice. If `invoice` is provided, the `target_pubkey`, `amount`, `payment_hash` can be omitted.

If `invoice` is not provided, the `target_pubkey`, `amount` must be provided, if `payment_hash` is not provided, the `payment_hash` will be generated by the node with a random preimage (means the `keysend` mode) in payment.

###### Returns

Return a `SendPaymentResult` object with the following fields:
- `payment_hash` (type: `Hash256`): The identifier of the payment, should be the same as the `payment_hash` in the request.
- `status` (type: `String`): The status of the payment, possible values are `created`, `inflight`, `success`, `failed`.
- `last_update_time` (type: `u128`): The last update time of the payment.
- `failed_error` (type: `Option<String>`): The error message if the payment failed.

<a id="get_payment"></a>
#### Method `get_payment`

Get the payment by the payment hash.

###### Params

- `payment_hash` (type: `Hash256`): The hash to use within the payment's HTLC.

###### Returns

If success, return a `SendPaymentResult` object with the following fields:
- `payment_hash` (type: `Hash256`): The identifier of the payment, should be the same as the `payment_hash` in the request.
- `status` (type: `String`): The status of the payment, possible values are `created`, `inflight`, `success`, `failed`.
- `last_update_time` (type: `u128`): The last update time of the payment.
- `failed_error` (type: `Option<String>`): The error message if the payment failed.

If the payment is not found, return error message.

### Module `Invoice`

RPC module for invoice management.

<a id="new_invoice"></a>
#### Method `new_invoice`

Generates a new invoice.

###### Params

* `amount` - The amount of CKB or UDT to request
* `currency` - The currency of the amount, either "CKB" or the UDT type script
* `description` - The description of the invoice, an optional parameter
* `expiry` - The expiry time of the invoice, an optional parameter
* `payment_preimage` - The payment preimage of the invoice

###### Returns

Returns the generated invoice string when the request is successful. Otherwise, returns an error message.

<a id="parse_invoice"></a>
#### Method `parse_invoice`

Parses an invoice string.

###### Params

* `invoice` - The invoice string to parse

###### Returns

* `invoice` - The parsed invoice object
    * `amount` - The amount of CKB or UDT requested
    * `currency` - The currency of the amount
    * `description` - The description of the invoice
    * `payment_hash` - The payment hash of the invoice

### Module `Peer`

RPC module for peer management.

<a id="connect_peer"></a>

#### Method `connect_peer`

Attempts to connect to a peer.

###### Params

* `address` - The address of the peer to connect to

###### Returns

Returns null when the request is successful. Otherwise, returns an error message.

<a id="disconnect_peer"></a>
#### Method `disconnect_peer`

Attempts to disconnect from a peer.

###### Params

* `peer_id` - The peer ID to disconnect from

###### Returns

Returns null when the request is successful. Otherwise, returns an error message.

### Module `Graph`

<a id="graph_nodes"></a>
#### Method `graph_nodes`
Get all the nodes in the network graph.

###### Params
* `limit`: The maximum number of nodes to return, an optional parameter
* `after`: Return the nodes after public key `after`, used for pagination, an optional parameter

###### Returns
* `nodes`: An array of node objects, each object contains the following fields:
    * `alias`: The alias of the node
    * `node_id`: The public key of the node
    * `addresses`: An array of addresses of the node
    * `timestamp`: The timestamp when the node was added to the graph, in milliseconds
    * `chain_hash`: The chain hash of the node, used to identify the network chain the node is on
    * `auto_accept_min_ckb_funding_amount`: The minimum CKB funding amount for auto-accepting a channel request
    * `udt_cfg_infos`: An array of UDT configuration objects, each object contains the following fields:
        * `name`: The name of the UDT
        * `script`: The type script configuration of the UDT, with feilds of
            * `code_hash`: The code hash of the UDT
            * `hash_type`: The hash type of the UDT
            * `args`: The arguments of the UDT
        * `auto_accept_amount`: The funding amount for the UDT to auto accept a channel request
        * `cell_deps`: The cell deps of the UDT, with fields of
            * `tx_hash`: The tx hash of the cell dep
            * `index`: The index of the cell dep
            * `dep_type`: The dep type of the cell dep
* `last_cursor`: The last public key of the returned nodes, used for pagination

<a id="graph_channels"></a>
#### Method `graph_channels`
Get all the channels in the network graph.

###### Params
* `limit`: The maximum number of channels to return, an optional parameter
* `after`: Return the channels after channel outpoint `after`, used for pagination, an optional parameter

###### Returns
* `channels`: An array of channel objects, each object contains the following fields:
    * `channel_outpoint`: The funding outpoint of the channel, used as the identifier of the channel
    * `funding_tx_block_number`: The funding transaction block number
    * `funding_tx_index`: The funding transaction index in the block
    * `node1`: The public key of the first node in the channel
    * `node2`: The public key of the second node in the channel
    * `last_updated_timestamp`: The timestamp when the channel was last updated, in milliseconds
    * `created_timestamp`: The timestamp when the channel was created, in milliseconds
    * `node1_to_node2_fee_rate`: The fee rate from the first node to the second node
    * `node2_to_node1_fee_rate`: The fee rate from the second node to the first node
    * `capacity`: The capacity of the channel
    * `chain_hash`: The chain hash of the channel, used to identify the network chain the channel is on
    * `udt_type_script` - The type script of the UDT to fund the channel with, an optional parameter


### Module `Info`

<a id="node_info"></a>
#### Method `node_info`

Get the node information.

###### Params
No

###### Returns

Returns a struct with these fields:

* `version`: The version of the node software.
* `commit_hash`: The commit hash of the node software.
* `public_key`: The public key of the node.
* `node_name`: The optional name of the node.
* `peer_id`: The peer ID of the node, serialized as a string.
* `addresses`: A list of multi-addresses associated with the node.
* `chain_hash`: The hash of the blockchain that the node is connected to.
* `open_channel_auto_accept_min_ckb_funding_amount`: The minimum CKB funding amount for automatically accepting open channel requests, serialized as a hexadecimal string.
* `auto_accept_channel_ckb_funding_amount`: The CKB funding amount for automatically accepting channel requests, serialized as a hexadecimal string.
* `tlc_locktime_expiry_delta`: The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
* `tlc_min_value`: The minimum value for Time-Locked Contracts (TLC), serialized as a hexadecimal string, `0` means no minimum value limit.
* `tlc_max_value`: The maximum value for Time-Locked Contracts (TLC), serialized as a hexadecimal string, `0` means no maximum value limit.
* `tlc_fee_proportional_millionths`: The fee proportional to the value of Time-Locked Contracts (TLC), expressed in millionths and serialized as a hexadecimal string.
* `channel_count`: The number of channels associated with the node, serialized as a hexadecimal string.
* `pending_channel_count`: The number of pending channels associated with the node, serialized as a hexadecimal string.
* `peers_count`: The number of peers connected to the node, serialized as a hexadecimal string.
* `network_sync_status`: The synchronization status of the node within the network, possible values are :
    * `NotRunning`: The syncing is not running, but we have all the information to start syncing.
    * `Running`: We should start running the syncing immediately or the syncing is already in progress.
    * `Done`: Syncing done, unless we restart the node, we don't have to sync again
* `udt_cfg_infos`: An array of UDT configuration objects, each object contains the following fields:
    * `name`: The name of the UDT
    * `script`: The type script configuration of the UDT, with feilds of
        * `code_hash`: The code hash of the UDT
        * `hash_type`: The hash type of the UDT
        * `args`: The arguments of the UDT
    * `auto_accept_amount`: The funding amount for the UDT to auto accept a channel request
    * `cell_deps`: The cell deps of the UDT, with fields of
        * `tx_hash`: The tx hash of the cell dep
        * `index`: The index of the cell dep
        * `dep_type`: The dep type of the cell dep