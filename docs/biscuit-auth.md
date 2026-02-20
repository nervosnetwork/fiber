# Biscuit Authentication in Fiber

This document provides an overview of Biscuit authentication, how it's integrated into Fiber's RPC system, and how to use it.

## 1. What is Biscuit Authentication?

[Biscuit] is a modern authorization token format designed for distributed verification. It's similar in concept to other bearer tokens like JWTs (JSON Web Tokens).

## 2. How Biscuit Auth Works in Fiber RPC

Biscuit is integrated into the Fiber RPC, it can be disabled when RPC is listen to private addr, but must be enabled when listen to public address. When enabled, it protects RPC endpoints by requiring a valid Biscuit token with permission for resources.

Here's a step-by-step breakdown of the process:

1.  **Client Request**: The client sends an RPC request and includes the Biscuit token in the `Authorization` HTTP header, formatted as a `Bearer` token. The token itself is Base64-encoded.

2.  **Middleware Interception**: On the server, the `BiscuitAuthMiddleware` intercepts every incoming RPC request before it reaches the actual method handler.

3.  **Token Extraction and Verification**: The middleware extracts the Base64-encoded token from the header, decodes it, and verifies its signature using the server's configured public key. If the signature is invalid, the request is immediately rejected.

4.  **Rule-based Authorization**: If the signature is valid, the middleware proceeds to the authorization step.
    *   Fiber maintains a predefined set of authorization rules for each RPC method. For example, the `open_channel` method requires a token with the `write("channels")` permission.
    *   The middleware builds a Biscuit **Authorizer**. This authorizer is loaded with the rule corresponding to the RPC method being called.
    *   It also adds contextual facts to the authorizer, such as the current time (e.g., `time(2023-10-27T10:00:00Z)`) and any parameters from the request. This allows for policies that depend on time or specific request data.

5.  **Policy Execution**: The authorizer then evaluates the token against the loaded rules and facts. It checks if the permissions granted in the token (the "authority" block and any attenuated blocks) satisfy the requirements of the RPC method's policy.

6.  **Access Control**:
    *   If the authorization check passes, the middleware forwards the request to the intended RPC method, and the operation proceeds.
    *   If the check fails (either due to insufficient permissions or a failed check like an expired timestamp), the middleware rejects the request with an "Unauthorized" error.

This entire process happens transparently for the RPC methods themselves. The logic is neatly contained within the middleware, ensuring that security is applied consistently across all protected endpoints.

The current rules for each RPC methods:

``` rust
// Cch 
rule("send_btc", r#"allow if write("cch");"#); 
rule("receive_btc", r#"allow if read("cch");"#); 
rule("get_cch_order", r#"allow if read("cch");"#); 
// channels 
rule("open_channel", r#"allow if write("channels");"#); 
rule("accept_channel", r#"allow if write("channels");"#); 
rule("abandon_channel", r#"allow if write("channels");"#); 
rule("list_channels", r#"allow if read("channels");"#); 
rule("shutdown_channel", r#"allow if write("channels");"#); 
rule("update_channel", r#"allow if write("channels");"#); 
// dev 
rule("commitment_signed", r#"allow if write("messages");"#); 
rule("add_tlc", r#"allow if write("channels");"#); 
rule("remove_tlc", r#"allow if write("channels");"#); 
rule(
    "check_channel_shutdown",
    r#"allow if write("channels");"#,
);
rule( 
    "submit_commitment_transaction", 
    r#"allow if write("chain");"#, 
); 
// graph 
rule("graph_nodes", r#"allow if read("graph");"#); 
rule("graph_channels", r#"allow if read("graph");"#); 

// info 
rule("node_info", r#"allow if read("node");"#); 
rule("new_invoice", r#"allow if write("invoices");"#); 
rule("parse_invoice", r#"allow if read("invoices");"#); 
rule("get_invoice", r#"allow if read("invoices");"#); 
rule("cancel_invoice", r#"allow if write("invoices");"#); 
rule("settle_invoice", r#"allow if write("invoices");"#); 

// payment 
rule("send_payment", r#"allow if write("payments");"#); 
rule("get_payment", r#"allow if read("payments");"#); 
rule("build_router", r#"allow if read("payments");"#); 
rule("send_payment_with_router", r#"allow if write("payments");"#); 

// peer 
rule("connect_peer", r#"allow if write("peers");"#); 
rule("disconnect_peer", r#"allow if write("peers");"#); 
rule("list_peers", r#"allow if read("peers");"#); 

// watchtower 
rule( 
    "create_watch_channel", 
    r#" 
    allow if write("watchtower"); 
    allow if right({channel_id}, "watchtower"); 
    "#, 
); 
rule( 
    "remove_watch_channel", 
    r#" 
    allow if write("watchtower"); 
    allow if right({channel_id}, "watchtower"); 
    "#, 
); 
rule( 
    "update_revocation", 
    r#" 
    allow if write("watchtower"); 
    allow if right({channel_id}, "watchtower"); 
    "#, 
); 
rule( 
    "update_local_settlement", 
    r#" 
    allow if write("watchtower"); 
    allow if right({channel_id}, "watchtower"); 
    "#, 
); 
rule("create_preimage", r#"allow if write("watchtower");"#); 
rule("remove_preimage", r#"allow if write("watchtower");"#); 
```

## 3. How to Configure Biscuit Auth in Fiber

Enabling Biscuit authentication on a Fiber node is a two-step process: first, you generate a cryptographic key pair, and second, you provide the public key to the Fiber server. This public key is used to verify the signatures of incoming Biscuit tokens, and its presence in the configuration automatically activates the authentication middleware.

### a. Generate a Key Pair

To sign and verify tokens, you need an Ed25519 key pair. You can generate one using the `biscuit-cli` tool.

Please refer to the [official Biscuit documentation][biscuit-doc] for usage.


``` bash
# make sure you use 0.6.0 version
cargo install biscuit-cli --vers 0.6.0-beta.2
```

```bash
biscuit keypair
```


This command will output a private and a public key.

```
Generating a new random keypair
Private key: ed25519-private/89d6c88919e5ca326fbb8d1cbef406df08c0620575376651d53008762dc81f45
Public key: ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79
```

**Important**: The **private key** is a secret and should be stored securely. It is used to sign and create new authorization tokens. The **public key** is what you will use to configure the Fiber server.

### b. Configure the Public Key

You can provide the public key to your Fiber node in one of the following ways:

#### Through the Configuration File

Add the public key to your Fiber configuration file (e.g., `config.yml`) under the `rpc` section:

```yaml
rpc:
  # ... other rpc settings
  biscuit_public_key: "ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79" # Your ed25519 public key string
```

#### Using Command-Line Arguments

When starting the `fiber-bin` executable, you can pass the public key as a command-line argument:

```bash
fiber-bin --rpc-biscuit-public-key "ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79"
```

#### Via Environment Variables

You can also configure it using an environment variable:

```bash
export RPC_BISCUIT_PUBLIC_KEY="ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79"
fiber-bin
```

If `biscuit_public_key` is not set, the RPC server will not require authentication. For security, Fiber will refuse to start on a public IP address if authentication is not enabled.

## 4. How to Sign an Auth Token

Once your server is configured for authentication, you need to create signed Biscuit tokens to access its RPC endpoints. This is done using the **private key** generated by `biscuit-cli`.

### a. Define Permissions

First, create a file (e.g., `permissions.bc`) to define the permissions for the token. These permissions are expressed as Datalog facts and checks, find details in the biscuit [documentation][biscuit-doc].

For example, to grant read access to peers, write access to payments, and add an expiration date:

```datalog
// Grant permissions for specific modules
read("peers");
write("payments");

// You can also add checks, like an expiration date.
// This check ensures the token is only valid before the specified UTC timestamp.
check if time($time), $time <= 2025-01-01T00:00:00Z;
```

### b. Generate the Token

Use the `biscuit generate` command with your private key and the permissions file to create the token.

```bash
biscuit generate --private-key ed25519-private/89d6c88919e5ca326fbb8d1cbef406df08c0620575376651d53008762dc81f45 permissions.bc
```

The command will output a long Base64-encoded string. This is your bearer token.

```
ErsBClEKBXBlZXJzCghwYXltZW50cxgDIgkKBwgAEgMYgAgiCQoHCAESAxiBCDImCiQKAggbEgYIBRICCAUaFgoECgIIBQoICgYggIvSuwYKBBoCCAISJAgAEiDAjoKKNTZpA61ImgoD5Q2sSbBjA3ixK1R2M65a2TOxbRpA4SF04LS5zD6OempqQObA2TTlTCANI7bZkDpl7JIecjR59GFoNtyKYak-_jXq2gDUHadj2I8SbJ0N-vN1RPslDSIiCiBR7VZ7ZJaPE4nhxUUpgpd5MXr3hqi0RfNERy3NE4KjaQ==
```

### c. Using the Token

You can now use this token in the `Authorization` header when making RPC calls to your Fiber node:

```
Authorization: Bearer ErsBClEKBXBlZXJzCghwYXltZW50cxgDIgkKBwgAEgMYgAgiCQoHCAESAxiBCDImCiQKAggbEgYIBRICCAUaFgoECgIIBQoICgYggIvSuwYKBBoCCAISJAgAEiDAjoKKNTZpA61ImgoD5Q2sSbBjA3ixK1R2M65a2TOxbRpA4SF04LS5zD6OempqQObA2TTlTCANI7bZkDpl7JIecjR59GFoNtyKYak-_jXq2gDUHadj2I8SbJ0N-vN1RPslDSIiCiBR7VZ7ZJaPE4nhxUUpgpd5MXr3hqi0RfNERy3NE4KjaQ==
```

This token grants exactly the permissions you defined and will be successfully verified by a Fiber server configured with the corresponding public key.

## 5. Test Examples

The Fiber codebase contains numerous tests that demonstrate the usage of Biscuit tokens. These are excellent resources for understanding the system.

### a. RPC Integration Tests

The file `crates/fiber-lib/src/fiber/tests/rpc.rs` contains integration-style tests for the RPC interface. Key examples include:

*   `test_rpc_basic_with_auth`: Shows how to make multiple authenticated RPC calls with a single token.
*   `test_rpc_auth_without_token`: Demonstrates that an unauthenticated request to a protected endpoint is rejected.
*   `test_rpc_auth_with_invalid_token`: Tests that a token signed by an incorrect private key is rejected.
*   `test_rpc_auth_with_wrong_permission`: Shows that a valid token with insufficient permissions is correctly denied access.

[Link to file: `crates/fiber-lib/src/fiber/tests/rpc.rs`](../crates/fiber-lib/src/fiber/tests/rpc.rs)

### b. Unit Tests for Authorization Logic

The file `crates/fiber-lib/src/rpc/biscuit.rs` contains unit tests that focus specifically on the token authorization logic. These are useful for seeing how different kinds of Datalog rules are handled.

*   `test_biscuit_auth`: Basic tests for checking `read` and `write` permissions.
*   `test_biscuit_auth_channel`: A more advanced example of granting permissions based on request parameters (in this case, a `channel_id`).
*   `test_biscuit_token_timeout`: Demonstrates how to create and verify a token with an expiration date.

[Link to file: `crates/fiber-lib/src/rpc/biscuit.rs`](../crates/fiber-lib/src/rpc/biscuit.rs)

[biscuit]: https://www.biscuitsec.org/
[biscuit-doc]: https://doc.biscuitsec.org/usage/command-line
