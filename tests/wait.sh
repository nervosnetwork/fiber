#!/usr/bin/env bash
set -euo pipefail

# Wait for the nodes to start, the initialization takes some time
# check all the ports are open

port_file=./tests/nodes/.ports
retry_count=0
while [ $retry_count -lt 100 ]; do
    if [ -f $port_file ]; then
        break
    else
        retry_count=$((retry_count + 1))
        echo "File $port_file not found. Retrying in 2 seconds..."
        sleep 2
    fi
done

ports=()
while IFS= read -r line; do
    ports+=("$line")
done < ./tests/nodes/.ports

echo "Checking if all ports are open ... ${ports[@]}"

try_number=120
count=0
while [ $count -lt $try_number ]; do
    all_open=true
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
        echo "Not all ports are open, waiting 3 seconds before retrying"
        sleep 3
    fi
done