# Development

## Run 3 nodes with default balances and deploy contracts to the devchain

Below command automatically start 3 FNN and 1 ckb node.
When we have not initialized the dev chain for ckb, this command will automatically do that.

```
./tests/nodes/start.sh
```

Running above command with environment variable `REMOVE_OLD_STATE` to `y` will remove all old states.
i.e.

```
REMOVE_OLD_STATE=y ./tests/nodes/start.sh
```

will start nodes in a clean state. This is useful in case like database schema are changed in development environment.

## (Re)Initialize a dev chain (optional)

We can (re)initialize the dev chain to transfer some balances from the default dev chain account (corresponding to NODE3) and deploy contracts to it. This is normally not needed as above command automatically do that. `-f` parameter may be used to forcefully clean all old state and reinitialize the dev chain.

```
./tests/deploy/init-dev-chain.sh [-f]
```

## Run some simple tests to the dev chain

```
cd tests/bruno
npm exec -- @usebruno/cli run e2e/open-use-close-a-channel -r --env test
```
