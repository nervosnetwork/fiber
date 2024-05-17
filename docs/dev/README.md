# Development

## Run 3 nodes with default balances and deploy contracts to the devchain

Below command automatically start 3 pcn nodes and 1 ckb node.
When we have not intialized the dev chain for ckb, this command will automatically do that.

```
./tests/nodes/start.sh
```

## (Re)Initialize a dev chain (optional)

We can (re)initialize the dev chain to transfer some balances from the default dev chain account (corresponding to NODE3) and deploy contracts to it. This is normally not needed as above command automatically do that. `-f` parameter may be used to forcefully clean all old state and reintialize the dev chain.

```
./tests/deploy/init-dev-chain.sh [-f]
```

## Run some simple tests to the dev chain

```
cd tests/bruno
npm exec -- @usebruno/cli run e2e/open-use-close-a-channel -r --env test
```