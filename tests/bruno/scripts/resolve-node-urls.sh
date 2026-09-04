#!/usr/bin/env bash
# Resolve the actual node endpoints from the generated node port record.
#
# CI provisioning (ON_GITHUB_ACTION=y) randomizes the node RPC/P2P ports:
# tests/deploy/udt-init (generate_nodes_config) writes the randomized ports
# into the generated tests/nodes/*/config.yml files and records the same
# values in tests/nodes/.ports, one port per line:
#
#   line 1: node1 P2P port      line 2: node1 RPC port
#   line 3: node2 P2P port      line 4: node2 RPC port
#   line 5: node3 P2P port      line 6: node3 RPC port
#
# Anything gating on the conventional default URLs (node RPC 21714/21715)
# would therefore hit dead endpoints. Source this file AFTER provisioning
# and BEFORE starting or gating on the nodes. When tests/nodes/.ports exists
# and its first four lines parse as valid ports, the following are exported
# from it:
#
#   NODE1_P2P_PORT NODE1_RPC_PORT NODE2_P2P_PORT NODE2_RPC_PORT
#   NODE1_RPC_URL  NODE2_RPC_URL  (http://127.0.0.1:<rpc port>)
#
# When the file is missing or malformed nothing is touched: callers keep
# their environment overrides or their defaults, so local runs without
# randomized provisioning behave exactly as before. A valid record wins over
# pre-set environment values - it is regenerated together with the node
# configs it describes, while an exported URL may go stale.

_node_ports_file="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)/../../../tests/nodes/.ports"

if [[ -f "$_node_ports_file" ]]; then
  _node_ports=()
  _node_port_line=""
  while IFS= read -r _node_port_line; do
    _node_ports+=("$_node_port_line")
  done <"$_node_ports_file"
  _node_ports_valid=1
  if (( ${#_node_ports[@]} < 4 )); then
    _node_ports_valid=0
  else
    for _node_port_line in "${_node_ports[0]}" "${_node_ports[1]}" "${_node_ports[2]}" "${_node_ports[3]}"; do
      if ! [[ "$_node_port_line" =~ ^[0-9]+$ ]] || (( _node_port_line < 1 || _node_port_line > 65535 )); then
        _node_ports_valid=0
        break
      fi
    done
  fi
  if (( _node_ports_valid )); then
    export NODE1_P2P_PORT="${_node_ports[0]}"
    export NODE1_RPC_PORT="${_node_ports[1]}"
    export NODE2_P2P_PORT="${_node_ports[2]}"
    export NODE2_RPC_PORT="${_node_ports[3]}"
    export NODE1_RPC_URL="http://127.0.0.1:$NODE1_RPC_PORT"
    export NODE2_RPC_URL="http://127.0.0.1:$NODE2_RPC_PORT"
  fi
  unset _node_ports _node_port_line _node_ports_valid
fi
unset _node_ports_file
