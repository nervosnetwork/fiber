#!/bin/sh
set -eu

contains_option() {
    short_option=$1
    long_option=$2
    shift 2

    while [ "$#" -gt 0 ]; do
        case "$1" in
            "$short_option"|"$long_option"|"$short_option="*|"$long_option="*)
                return 0
                ;;
        esac
        shift
    done

    return 1
}

get_option_value() {
    short_option=$1
    long_option=$2
    shift 2

    while [ "$#" -gt 0 ]; do
        case "$1" in
            "$short_option"|"$long_option")
                shift
                if [ "$#" -gt 0 ]; then
                    printf '%s\n' "$1"
                fi
                return 0
                ;;
            "$short_option="*|"$long_option="*)
                printf '%s\n' "${1#*=}"
                return 0
                ;;
        esac
        shift
    done

    return 1
}

is_help_or_version() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -h|--help|-V|--version)
                return 0
                ;;
        esac
        shift
    done

    return 1
}

ensure_default_config() {
    config_path=$1
    config_template=${FIBER_CONFIG_TEMPLATE:-/usr/local/share/fiber/config/testnet/config.yml}

    mkdir -p "$(dirname "$config_path")"
    if [ ! -f "$config_path" ]; then
        cp "$config_template" "$config_path"
        echo "Copied bundled config template to $config_path" >&2
    fi
}

default_base_dir=${FIBER_HOME:-/fiber}

if [ "$#" -eq 0 ]; then
    set -- fnn
fi

if [ "${1#-}" != "$1" ]; then
    set -- fnn "$@"
fi

if [ "$1" = "fnn" ] && ! is_help_or_version "$@"; then
    base_dir=$(get_option_value "-d" "--dir" "$@" || true)
    if [ -z "$base_dir" ]; then
        base_dir=$default_base_dir
        mkdir -p "$base_dir" "$base_dir/ckb"
        set -- "$@" -d "$base_dir"
    fi

    if ! contains_option "-c" "--config" "$@"; then
        config_path=${FIBER_CONFIG:-$base_dir/config.yml}
        ensure_default_config "$config_path"
        set -- "$@" -c "$config_path"
    fi

    if [ -z "${FIBER_SECRET_KEY_PASSWORD:-}" ]; then
        echo "FIBER_SECRET_KEY_PASSWORD must be set when starting fnn in the container." >&2
        exit 1
    fi
fi

exec "$@"
