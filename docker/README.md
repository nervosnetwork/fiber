# Docker

Fiber release tags publish official container images to Docker Hub at `nervos/fiber` and mirror the same tags to GHCR at `ghcr.io/nervosnetwork/fiber`. Release tags are published as image tags with the same version, and stable releases also update the `latest` tag.

## Build locally

```bash
docker build -f docker/Dockerfile -t fiber:local .
```

## Run the published image

```bash
mkdir -p ./fiber-node/ckb
# Put your CKB private key in ./fiber-node/ckb/key before starting the node.

docker run --rm -it \
  --name fiber-node \
  -e FIBER_SECRET_KEY_PASSWORD='YOUR_PASSWORD' \
  -e RUST_LOG='info' \
  -v "$(pwd)/fiber-node:/fiber" \
  -p 8228:8228 \
  nervos/fiber:<release-tag>
```

If you prefer GHCR, replace the image reference with `ghcr.io/nervosnetwork/fiber:<release-tag>`.

On first start, the image copies the bundled testnet config to `/fiber/config.yml` if the file does not already exist. To bootstrap from the bundled mainnet config instead, set `FIBER_CONFIG_TEMPLATE=/usr/local/share/fiber/config/mainnet/config.yml`.

The image also includes `fnn-cli` and `fnn-migrate` for administration and data migration tasks.

The RPC service listens on `127.0.0.1:8227` in the bundled configs, so publishing port `8227` from the container is not enough by itself. If you need remote RPC access, update `rpc.listening_addr` in your mounted config first.

## Use `fnn-cli` with Docker

If the node is already running in a container, the simplest way to use `fnn-cli` is to execute it inside the same container so it can talk to the default `127.0.0.1:8227` RPC endpoint:

```bash
docker exec -it fiber-node fnn-cli info
docker exec -it fiber-node fnn-cli channel list_channels
docker exec -it fiber-node fnn-cli payment list_payments
```

For interactive mode:

```bash
docker exec -it fiber-node fnn-cli
```

If you enabled RPC authentication, pass the token the same way as on bare metal:

```bash
docker exec -it fiber-node fnn-cli --auth-token 'YOUR_TOKEN' info
```

The full CLI reference is available in [../crates/fiber-cli/README.md](../crates/fiber-cli/README.md).

## Migrate data with Docker

When upgrading between versions that require a storage migration, stop the node container first and back up the mounted data directory. With the default Docker layout shown above, the store path is `/fiber/fiber/store` inside the container.

Run the migration tool against the same mounted directory:

```bash
docker run --rm \
  -v "$(pwd)/fiber-node:/fiber" \
  nervos/fiber:<new-release-tag> \
  fnn-migrate -p /fiber/fiber/store
```

After the migration finishes, start the new image version again with the same bind mount. If your config overrides the default store path, replace `/fiber/fiber/store` with that custom path.
