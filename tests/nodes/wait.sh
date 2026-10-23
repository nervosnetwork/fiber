#!/usr/bin/env bash
set -euo pipefail

# Wait for the nodes to start, the initialization takes some time
# check all the ports are open

port_file=./tests/nodes/.ports
extra_port_file=./tests/nodes/.extra-ports
retry_count=0
while [ $retry_count -lt 100 ]; do
    if [ -f $port_file ]; then
        break
    else
        retry_count=$((retry_count + 1))
        echo "File $port_file not found. Retrying in 30 seconds..."
        sleep 30
    fi
done

if [ ! -f $port_file ]; then
    echo "File $port_file not found after retries, exiting with status 1"
    exit 1
fi

# Re-read on every attempt so LSP start.sh can publish the SDK agent
# control port in .extra-ports independently of the remapped node ports.
read_ports() {
    ports=()
    local file line extra
    for file in "$port_file" "$extra_port_file"; do
        if [ -f "$file" ]; then
            while IFS= read -r line; do
                [ -n "$line" ] || continue
                ports+=("$line")
            done <"$file"
        fi
    done
    if [ -n "${EXTRA_WAIT_PORTS:-}" ]; then
        for extra in $EXTRA_WAIT_PORTS; do
            ports+=("$extra")
        done
    fi
}

try_number=120
count=0
while [ $count -lt $try_number ]; do
    read_ports
    echo "Checking if all ports are open ... ${ports[*]}"
    all_open=true
    if [ ${#ports[@]} -eq 0 ]; then
        echo "No ports listed yet ..."
        all_open=false
    fi
    for port in "${ports[@]}"; do
        if ! nc -z 127.0.0.1 $port; then
            echo "Port $port is not open yet ..."
            all_open=false
            break
        fi
    done
    if $all_open; then
        echo "All ports are open now ..."
        exit 0
    else
        count=$((count + 1))
        if [ $count -eq $try_number ]; then
          echo "Reached maximum number of tries ($try_number), exiting with status 1"
            exit 1
        fi
        sleep 10
    fi
done