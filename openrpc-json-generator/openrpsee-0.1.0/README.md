# Openrpsee

**Openrpsee** is a helper library for building **OpenRPC documents** and implementing the `rpc.discover` endpoint.

Its main goal is to centralize common functionality used across the Zcash **Z3 stack** (**Zebra**, **Zallet**, and **Zaino**) while remaining generally useful for any project that builds JSON-RPC servers using the `jsonrpsee` ecosystem.

## Overview

Openrpsee provides utilities to:

- Extract RPC method definitions from Rust traits
- Generate a structured representation of those methods at build time
- Produce standards-compliant OpenRPC documents at runtime
- Expose those documents through a `rpc.discover` RPC endpoint

## Generating OpenRPC Metadata

The `generate_openrpc` function parses the given RPC traits, extracts all method definitions, and builds a map containing the metadata required to generate an OpenRPC document.
This map is then written as a Rust source file into the specified output directory.

### Examples

- Zallet: https://github.com/zcash/wallet/blob/openrpsee/zallet/build.rs#L69
- Zebra: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/build.rs#L119

## Including the Generated File

The generated Rust file is intended to be included from your project’s `methods.rs` (or equivalent) module.

### Examples:

- Zallet: https://github.com/zcash/wallet/blob/openrpsee/zallet/src/components/json_rpc/methods/openrpc.rs#L9
- Zebra: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/src/methods.rs#L138

## Generating the OpenRPC Document

The `openrpc` module provided by this crate consumes the generated method map and produces an OpenRPC document.
This document is typically returned by an implementation of the `rpc.discover` RPC method.

### Examples:

- Zallet: https://github.com/zcash/wallet/blob/openrpsee/zallet/src/components/json_rpc/methods/openrpc.rs
- Zebra: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/src/methods.rs#L2983-L3002

## Argument Documentation

All RPC arguments are expected to be described using constants. These constants are used when generating the OpenRPC schema.

Examples:

- Zallet: https://github.com/zcash/wallet/blob/openrpsee/zallet/src/components/json_rpc/methods/get_new_account.rs#L36-L37
- Zebra: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/src/methods.rs#L142-L167

## Produced OpenRPC Documents

Live OpenRPC documents generated using this library can be found at:

- Zallet: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/zallet_openrpc_schema.json
- Zebra: https://github.com/ZcashFoundation/zebra/blob/openrpc/zebra-rpc/zebra_openrpc_schema.json
- Z3: https://github.com/oxarbitrage/z3/blob/openrpc-router/rpc-router/z3_merged.json

## Playground

Interactive playgrounds exposing the generated OpenRPC schemas:

- Zallet: https://playground.open-rpc.org/?uiSchema[appBar][ui:title]=Zcash&uiSchema[appBar][ui:logoUrl]=https://z.cash/wp-content/uploads/2023/03/zcash-logo.gif&schemaUrl=https://raw.githubusercontent.com/ZcashFoundation/zebra/refs/heads/openrpc/zebra-rpc/zallet_openrpc_schema.json&uiSchema[appBar][ui:splitView]=false&uiSchema[appBar][ui:edit]=false&uiSchema[appBar][ui:input]=false&uiSchema[appBar][ui:examplesDropdown]=false&uiSchema[appBar][ui:transports]=false
- Zebra: https://playground.open-rpc.org/?uiSchema[appBar][ui:title]=Zcash&uiSchema[appBar][ui:logoUrl]=https://z.cash/wp-content/uploads/2023/03/zcash-logo.gif&schemaUrl=https://raw.githubusercontent.com/ZcashFoundation/zebra/refs/heads/openrpc/zebra-rpc/zebra_openrpc_schema.json&uiSchema[appBar][ui:splitView]=false&uiSchema[appBar][ui:edit]=false&uiSchema[appBar][ui:input]=false&uiSchema[appBar][ui:examplesDropdown]=false&uiSchema[appBar][ui:transports]=false
- Z3: https://playground.open-rpc.org/?uiSchema[appBar][ui:title]=Zcash&uiSchema[appBar][ui:logoUrl]=https://z.cash/wp-content/uploads/2023/03/zcash-logo.gif&schemaUrl=https://raw.githubusercontent.com/oxarbitrage/z3/refs/heads/openrpc-router/rpc-router/z3_merged.json&uiSchema[appBar][ui:splitView]=false&uiSchema[appBar][ui:edit]=false&uiSchema[appBar][ui:input]=false&uiSchema[appBar][ui:examplesDropdown]=false&uiSchema[appBar][ui:transports]=false
