# How to Back Up Fiber Data

This document explains how to securely back up Fiber node data, including the entire node data directory and the `FIBER_SECRET_KEY_PASSWORD`, to ensure the safety of funds, integrity of node identity, and smooth node migration or upgrades. **Backing up plaintext private keys is not recommended**, as plaintext private keys (e.g., exported via `ckb-cli`) pose a risk of leakage.

## Table of Contents

1. [Importance of Encrypted Private Key and FIBER_SECRET_KEY_PASSWORD](#1-importance-of-encrypted-private-key-and-fiber_secret_key_password)
2. [Importance of Network ID Key](#2-importance-of-network-id-key)
3. [Backing Up Node Data](#3-backing-up-node-data)
4. [Restoring Node Data](#4-restoring-node-data)
5. [Handling Forgotten Passwords](#5-handling-forgotten-passwords)

## 1. Importance of Encrypted Private Key and FIBER_SECRET_KEY_PASSWORD

When a Fiber node starts for the first time, the system encrypts the private key in the `ckb/key` file using the `FIBER_SECRET_KEY_PASSWORD`. **The encrypted private key file (`ckb/key`) and the password are critical for recovering funds**. If the password is lost, the node will fail to start and display an error:

```
Secret key file error: decryption failed: aead::Error
```

Example of a startup failure log:

```
2025-05-14T06:49:05.206118Z  WARN fnn::store::db_migrate: no need to migrate, everything is OK ...
    at crates/fiber-lib/src/store/db_migrate.rs:72
    in fnn::node with node: ""

2025-05-14T06:49:05.242598Z  INFO fnn::ckb::contracts: Creating ContractsContext for dev
    at crates/fiber-lib/src/ckb/contracts.rs:189
    in fnn::node with node: ""

Error: Exit because failed to start ckb actor: Actor panicked during startup 'Secret key file error: decryption failed: aead::Error'
```

**Important Notes**:

- **Encrypted Private Key**: Stored in `ckb/key`, this file is encrypted after the first startup and is automatically included when backing up the node data directory. Losing this file will make funds unrecoverable.
- **FIBER_SECRET_KEY_PASSWORD**: The password used to decrypt the private key must be recorded and stored securely (e.g., in a password manager).
- **Avoid Backing Up Plaintext Private Keys**: Plaintext private keys exported via `ckb-cli` are unencrypted and prone to theft, posing a security risk.

## 2. Importance of Network ID Key

When a Fiber node starts for the first time, it generates a random network ID key, stored in the `fiber/sk` file. **This file serves as the node's identity for establishing connections and verifying identity with other nodes**. It is automatically included when backing up the entire node data directory.

**Important Notes**:

- **Network ID Key**: Stored in `fiber/sk`, losing this file will prevent other nodes from recognizing the node, affecting network communication.
- During migration or redeployment, the same `fiber/sk` file must be retained to maintain consistent node identity.

## 3. Backing Up Node Data

Currently, when upgrading or migrating a node, the entire working directory (`node1`), including the `ckb` and `fiber` directories, must be backed up. **The node must be stopped before backing up**, otherwise, an outdated version of the `commitment tx` may be backed up, leading to serious issues such as:

- Signature errors when interacting with other nodes.
- Potential loss of funds if an outdated `commitment tx` is used to forcibly close a channel.

The working directory (node1) contains the following structure:
```
node1
├── ckb
│   └── key
├── config.yml
└── fiber
    ├── sk
    └── store
        ├── 020941.sst
        ├── 020942.log
        ├── 020943.sst
        ├── CURRENT
        ├── IDENTITY
        ├── LOCK
        ├── LOG
        ├── MANIFEST-000005
        └── OPTIONS-000007
```

**Backup Steps**:

1. Stop the node:
    
    ```bash
    # Check running fnn processes
    ps aux | grep fnn
    # Example output
    ckb       832623  0.0  0.0   7008  2432 pts/2    S+   06:42   0:00 grep --color=auto fnn
    ckb      3585187  0.7  2.4 756932 194052 ?       Sl   May07  71:52 ./fnn -c ./node1/config.yml -d ./node1
    
    # Terminate the process (replace 3585187 with the actual process ID)
    kill 3585187
    ```

2. Back up the node data:
    
    ```bash
    tar -zcvf node1.tar.gz node1
    ```

3. Store the backup file `node1.tar.gz` in a secure location.

**Non-Stop Backup Plan**:

To avoid the inconvenience of stopping the node for backups, we plan to introduce a non-stop backup feature for RocksDB data via an RPC interface in future versions. This feature will allow safe data backups while the node is running, reducing service interruptions. Please follow the release notes for updates on this feature.

## 4. Restoring Node Data

To restore node data, extract the backup file and start the node with the correct `FIBER_SECRET_KEY_PASSWORD`.

**Restore Steps**:

1. Extract the backup file:
    
    ```bash
    tar -zxvf node1.tar.gz
    ```

2. Verify the backup contents: Ensure the `node1/ckb/key` and `node1/fiber/sk` files exist and are unmodified.
3. Start the node:
    
    ```bash
    FIBER_SECRET_KEY_PASSWORD='YOUR_PASSWORD' ./fnn -c ./node1/config.yml -d ./node1
    ```

## 5. Handling Forgotten Passwords

If the `FIBER_SECRET_KEY_PASSWORD` is forgotten but a plaintext private key is retained (not recommended but may have been stored separately), you can reset the password and recover the node. **Note**: This method only works if you have the plaintext private key, and you should re-encrypt the private key as soon as possible.

**Recovery Steps**:

1. Extract the backup file:
    
    ```bash
    tar -zxvf node1.tar.gz
    ```

2. Delete the encrypted `ckb` directory:
    
    ```bash
    rm -rf node1/ckb
    ```

3. Create a new `ckb` directory and write the plaintext private key:
    
    ```bash
    mkdir node1/ckb
    echo "NODE1_PRIVATE_KEY" >> node1/ckb/key
    ```
    
    > Note: Replace `NODE1_PRIVATE_KEY` with the actual plaintext private key.

4. Start the node with a new password:
    
    ```bash
    FIBER_SECRET_KEY_PASSWORD='YOUR_NEW_PASSWORD' ./fnn -c ./node1/config.yml -d ./node1
    ```

5. **Re-encrypt the Private Key**: After the node starts, the `ckb/key` file will be encrypted with the new password. Ensure the new `FIBER_SECRET_KEY_PASSWORD` is backed up.
6. **Retain the Network ID Key**: Ensure the `node1/fiber/sk` file is not deleted or modified to maintain node identity consistency.

## Notes

- **Fund Safety**: Losing the encrypted private key (`ckb/key`) or the password will make funds unrecoverable. Always back up node data carefully.
- **Node Identity**: Losing the network ID key (`fiber/sk`) will prevent the node from being recognized, affecting network communication.
- **Avoid Plaintext Private Keys**: Unless absolutely necessary (e.g., lost password with no other backup), do not store or use plaintext private keys.
- **Stop the Node**: Ensure the node is stopped before backing up or manipulating data.
- **Verify Backups**: Regularly check the integrity of backup files to ensure they can be extracted and used.
- **Secure Storage**: Store the backup file (`node1.tar.gz`) and password in an encrypted device or offline environment to prevent leakage.
