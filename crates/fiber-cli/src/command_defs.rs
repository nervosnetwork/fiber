// Command definitions for fnn-cli.
//
// This file is parsed by build.rs to generate command() and execute() functions.
// It is also compiled normally as a module, but the constants are only consumed
// by the build script — hence the dead_code allow on the mod declaration.
//
// Format for each command group tuple:
//   (group_name, about, none_action, &[(subcommand_name, about, ParamsType, ResultType)])
//
// Subcommand conventions:
//   - ParamsType = "()" means no params (uses call_typed_no_params)
//   - ResultType = "()" means the RPC returns raw Value (no typed deserialization)
//   - ResultType = "SomeType" means deserialize into that type, then serde_json::to_value
//   - The RPC method name is always identical to the subcommand_name.
//
// NoneAction conventions (what happens when no subcommand is given):
//   - "help"                    → print help and return Value::Null
//   - "call:method_name"        → call the named subcommand (must have no params)
//   - "default:method_name"     → call the named subcommand with Default::default() params

/// Type alias for a command group definition tuple, to avoid clippy::type_complexity.
/// (group_name, about, none_action, &[(subcommand_name, about, ParamsType, ResultType)])
type CommandGroup = (
    &'static str,
    &'static str,
    &'static str,
    &'static [Subcommand],
);

/// Type alias for a single subcommand tuple.
/// (name, about, ParamsType, ResultType)
type Subcommand = (&'static str, &'static str, &'static str, &'static str);

/// Node information commands.
pub const INFO_COMMANDS: CommandGroup = (
    "info",
    "Get node information",
    "call:node_info",
    &[(
        "node_info",
        "Get the node information",
        "()",
        "NodeInfoResult",
    )],
);

/// Peer management commands.
pub const PEER_COMMANDS: CommandGroup = (
    "peer",
    "Manage peer connections",
    "help",
    &[
        (
            "connect_peer",
            "Connect to a peer",
            "ConnectPeerParams",
            "()",
        ),
        (
            "disconnect_peer",
            "Disconnect from a peer",
            "DisconnectPeerParams",
            "()",
        ),
        (
            "list_peers",
            "List connected peers",
            "()",
            "ListPeersResult",
        ),
    ],
);

/// Payment channel management commands.
pub const CHANNEL_COMMANDS: CommandGroup = (
    "channel",
    "Manage payment channels",
    "help",
    &[
        (
            "open_channel",
            "Open a channel with a peer",
            "OpenChannelParams",
            "OpenChannelResult",
        ),
        (
            "accept_channel",
            "Accept a channel opening request",
            "AcceptChannelParams",
            "()",
        ),
        (
            "abandon_channel",
            "Abandon a channel",
            "AbandonChannelParams",
            "()",
        ),
        (
            "list_channels",
            "List all channels",
            "ListChannelsParams",
            "ListChannelsResult",
        ),
        (
            "shutdown_channel",
            "Shut down a channel",
            "ShutdownChannelParams",
            "()",
        ),
        (
            "update_channel",
            "Update a channel's parameters",
            "UpdateChannelParams",
            "()",
        ),
    ],
);

/// Payment management commands.
pub const PAYMENT_COMMANDS: CommandGroup = (
    "payment",
    "Manage payments",
    "help",
    &[
        (
            "send_payment",
            "Send a payment to a peer",
            "SendPaymentCommandParams",
            "()",
        ),
        (
            "get_payment",
            "Retrieve a payment by payment hash",
            "GetPaymentCommandParams",
            "GetPaymentCommandResult",
        ),
        (
            "list_payments",
            "List all payments",
            "ListPaymentsParams",
            "ListPaymentsResult",
        ),
        (
            "build_router",
            "Build a payment router with specified hops",
            "BuildRouterParams",
            "()",
        ),
        (
            "send_payment_with_router",
            "Send a payment with a manually specified router",
            "SendPaymentWithRouterParams",
            "()",
        ),
    ],
);

/// Invoice management commands.
pub const INVOICE_COMMANDS: CommandGroup = (
    "invoice",
    "Manage invoices",
    "help",
    &[
        (
            "new_invoice",
            "Generate a new invoice",
            "NewInvoiceParams",
            "InvoiceResult",
        ),
        (
            "parse_invoice",
            "Parse an encoded invoice",
            "ParseInvoiceParams",
            "ParseInvoiceResult",
        ),
        (
            "get_invoice",
            "Retrieve an invoice by payment hash",
            "InvoiceParams",
            "GetInvoiceResult",
        ),
        ("cancel_invoice", "Cancel an invoice", "InvoiceParams", "()"),
        (
            "settle_invoice",
            "Settle an invoice by providing the preimage",
            "SettleInvoiceParams",
            "()",
        ),
    ],
);

/// Development/debug commands.
pub const DEV_COMMANDS: CommandGroup = (
    "dev",
    "Development/debug commands (not for production use)",
    "help",
    &[
        (
            "commitment_signed",
            "Send a commitment_signed message to a peer",
            "CommitmentSignedParams",
            "()",
        ),
        (
            "add_tlc",
            "Add a TLC to a channel",
            "AddTlcParams",
            "AddTlcResult",
        ),
        (
            "remove_tlc",
            "Remove a TLC from a channel",
            "RemoveTlcParams",
            "()",
        ),
        (
            "submit_commitment_transaction",
            "Submit a commitment transaction to the chain",
            "SubmitCommitmentTransactionParams",
            "SubmitCommitmentTransactionResult",
        ),
        (
            "check_channel_shutdown",
            "Manually trigger CheckShutdownTx on a channel",
            "CheckChannelShutdownParams",
            "()",
        ),
    ],
);

/// Network graph query commands.
pub const GRAPH_COMMANDS: CommandGroup = (
    "graph",
    "Query the network graph",
    "help",
    &[
        (
            "graph_nodes",
            "Get the list of nodes in the network graph",
            "GraphNodesParams",
            "GraphNodesResult",
        ),
        (
            "graph_channels",
            "Get the list of channels in the network graph",
            "GraphChannelsParams",
            "GraphChannelsResult",
        ),
    ],
);

/// Cross-chain hub commands.
pub const CCH_COMMANDS: CommandGroup = (
    "cch",
    "Cross-chain hub operations",
    "help",
    &[
        (
            "send_btc",
            "Create a CCH order for a BTC Lightning payee",
            "SendBTCParams",
            "CchOrderResponse",
        ),
        (
            "receive_btc",
            "Create a CCH order for a CKB Fiber payee",
            "ReceiveBTCParams",
            "CchOrderResponse",
        ),
        (
            "get_cch_order",
            "Get a CCH order by payment hash",
            "GetCchOrderParams",
            "CchOrderResponse",
        ),
    ],
);

/// Watchtower service commands.
pub const WATCHTOWER_COMMANDS: CommandGroup = (
    "watchtower",
    "Watchtower service commands",
    "help",
    &[
        (
            "create_watch_channel",
            "Create a new watched channel",
            "CreateWatchChannelParams",
            "()",
        ),
        (
            "remove_watch_channel",
            "Remove a watched channel",
            "RemoveWatchChannelParams",
            "()",
        ),
        (
            "update_revocation",
            "Update revocation data for a watched channel",
            "UpdateRevocationParams",
            "()",
        ),
        (
            "update_pending_remote_settlement",
            "Update pending remote settlement data for a watched channel",
            "UpdatePendingRemoteSettlementParams",
            "()",
        ),
        (
            "update_local_settlement",
            "Update local settlement data for a watched channel",
            "UpdateLocalSettlementParams",
            "()",
        ),
        (
            "create_preimage",
            "Store a preimage associated with a payment hash",
            "CreatePreimageParams",
            "()",
        ),
        (
            "remove_preimage",
            "Remove a stored preimage by payment hash",
            "RemovePreimageParams",
            "()",
        ),
    ],
);

/// Profiling commands.
pub const PROF_COMMANDS: CommandGroup = (
    "prof",
    "Profiling commands (requires pprof feature)",
    "default:pprof",
    &[(
        "pprof",
        "Collect a CPU profile and write a flamegraph SVG to disk",
        "PprofParams",
        "PprofResult",
    )],
);

/// All command groups in registration order.
pub const ALL_COMMAND_GROUPS: &[&str] = &[
    "INFO_COMMANDS",
    "PEER_COMMANDS",
    "CHANNEL_COMMANDS",
    "INVOICE_COMMANDS",
    "PAYMENT_COMMANDS",
    "GRAPH_COMMANDS",
    "CCH_COMMANDS",
    "DEV_COMMANDS",
    "WATCHTOWER_COMMANDS",
    "PROF_COMMANDS",
];
