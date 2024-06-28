The binaries within this directory are used to verify if the transaction built from our code base can pass ckb-vm verification.

The source code of these binaries are from the following repo [cfn-scripts](https://github.com/nervosnetwork/cfn-scripts) with commit 701d8c8a08790dd61c64f695aaa5fbed22e4b8ad.

We copied the following binaries from https://github.com/nervosnetwork/cfn-scripts/tree/main/deps

- auth
- simple_udt
- xudt_rce

and built the following binaries

- funding-lock
- commitment-lock