The binaries within this directory are used to verify if the transaction built from our code base can pass ckb-vm verification.

The source code of these binaries are from the following repo [fiber-scripts](https://github.com/nervosnetwork/fiber-scripts) with commit cc6c70dcd1b23fca020dc93a371c2d84e29a9a34.

We copied the following binaries from https://github.com/nervosnetwork/fiber-scripts/tree/main/deps

- auth
- simple_udt

The following binaries are built from https://github.com/nervosnetwork/ckb-production-scripts with commit 410b16c499a8888781d9ab03160eeef93182d8e6.

- xudt_rce

and built the following binaries

- funding-lock
- commitment-lock

The `liquidity-lock` binary is built from
[fiber-scripts](https://github.com/nervosnetwork/fiber-scripts) at commit
`4da5d299fe449bb799bf057e0bb1f02c8fe7d101` with:

```sh
mkdir -p build/release
make -C contracts/liquidity-lock build TOP="$(pwd)/" BUILD_DIR=build/release
shasum -a 256 build/release/liquidity-lock
```

Its expected SHA-256 is
`b7bfe051b6062f1ebb814ddf392406f86df9f403280eb48ec93a4d141c72dc4e`.
