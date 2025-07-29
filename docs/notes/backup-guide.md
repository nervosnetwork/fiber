# Fiber Node Backup Guide

This guide explains how to securely back up Fiber node data to safeguard funds, maintain node identity, and ensure smooth migration or upgrades. **Never store plaintext private keys**, as they risk theft if exposed.

## Table of Contents

1. [Why Back Up?](#1-why-back-up)
2. [Backing Up Node Data](#2-backing-up-node-data)
3. [Restoring Node Data](#3-restoring-node-data)
4. [Handling Forgotten Passwords](#4-handling-forgotten-passwords)

## 1. Why Back Up?

Backups protect:
- **Funds**: The encrypted private key (`ckb/key`) and its password (`FIBER_SECRET_KEY_PASSWORD`) are critical for accessing funds. Losing either makes funds unrecoverable.
- **Node Identity**: The network ID key (`fiber/sk`) allows other nodes to recognize your node. Losing it disrupts network communication.

**Warning**: Always stop the node before backing up to prevent data corruption, which could cause errors or loss of funds.

## 2. Backing Up Node Data

Back up the entire node directory (`fiber-dir`), which includes:
- `ckb/key` (encrypted private key)
- `fiber/sk` (network ID key)
- `fiber/store` (database files)
- `config.yml` (configuration)

**Steps**:
1. **Stop the Node**:
   ```bash
   # Check for running node processes
   ps aux | grep fnn
   # Example output: ckb 3585187 0.7 2.4 756932 194052 ? Sl May07 71:52 ./fnn -c ./fiber-dir/config.yml -d ./fiber-dir
   # Terminate it (replace 3585187 with your process ID)
   kill 3585187
   ```
2. **Create Backup**:
   ```bash
   tar -zcvf fiber-dir.tar.gz fiber-dir
   ```
3. **Store Securely**: Save `fiber-dir.tar.gz` and the `FIBER_SECRET_KEY_PASSWORD` in an offline, encrypted environment (e.g., an encrypted drive or password manager).

**Note**: A future update will allow backups without stopping the node. Check release notes for updates.

## 3. Restoring Node Data

**Steps**:
1. **Extract Backup**:
   ```bash
   tar -zxvf fiber-dir.tar.gz
   ```
2. **Start Node**:
   ```bash
   FIBER_SECRET_KEY_PASSWORD='YOUR_PASSWORD' ./fnn -c ./fiber-dir/config.yml -d ./fiber-dir
   ```

**Warning**: Using the wrong password will trigger a startup error (`Secret key file error: decryption failed`). Double-check the password before starting.

## 4. Handling Forgotten Passwords

If you lose the `FIBER_SECRET_KEY_PASSWORD` but have a plaintext private key (not recommended), you can reset the password.

**Steps**:
1. **Extract Backup**:
   ```bash
   tar -zxvf fiber-dir.tar.gz
   ```
2. **Remove Encrypted Key**:
   ```bash
   rm -rf fiber-dir/ckb
   ```
3. **Add Plaintext Key**:
   ```bash
   mkdir fiber-dir/ckb
   echo "YOUR_PLAINTEXT_KEY" > fiber-dir/ckb/key
   ```
4. **Start Node with New Password**:
   ```bash
   FIBER_SECRET_KEY_PASSWORD='NEW_PASSWORD' ./fnn -c ./fiber-dir/config.yml -d ./fiber-dir
   ```
5. **Secure the Key**: The `ckb/key` file will be re-encrypted with the new password. Store the new password securely.
