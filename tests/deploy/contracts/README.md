The binaries within this directory are used to verify if the transaction built from our code base can pass ckb-vm verification.

The source code of these binaries are from the following repo [fiber-scripts](https://github.com/nervosnetwork/fiber-scripts) with commit 701d8c8a08790dd61c64f695aaa5fbed22e4b8ad.

We copied the following binaries from https://github.com/nervosnetwork/fiber-scripts/tree/main/deps

- auth
- simple_udt

The following binaries are built from https://github.com/nervosnetwork/ckb-production-scripts with commit 410b16c499a8888781d9ab03160eeef93182d8e6.

- xudt_rce

and built the following binaries

- funding-lock
- commitment-lock
