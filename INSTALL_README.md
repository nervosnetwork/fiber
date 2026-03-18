# FNN 安装脚本

这是两个用于快速部署 Fiber Network Node (FNN) 的脚本。

## 脚本说明

### 1. install-fnn.sh - Linux/macOS 完整安装脚本

自动下载/编译 FNN 二进制文件并配置节点。

**使用方法：**

```bash
# 使用默认设置（安装到 ./my-fnn，使用 testnet）
./install-fnn.sh

# 指定安装目录和网络
./install-fnn.sh /path/to/node mainnet
```

**功能：**
- 自动检测平台 (Linux/macOS, x86_64/arm64)
- 下载预编译的 FNN 二进制文件或从源码编译
- 自动下载安装 ckb-cli（如未安装）
- 下载对应网络的配置文件
- 使用 ckb-cli 创建或导入 CKB 账户
- 导出私钥并设置权限
- 创建启动脚本

### 2. install-fnn.ps1 - Windows PowerShell 安装脚本

Windows 平台的完整安装脚本。

**使用方法：**

```powershell
# 使用默认设置（安装到 .\my-fnn，使用 testnet）
.\install-fnn.ps1

# 指定安装目录和网络
.\install-fnn.ps1 -InstallDir "C:\my-fnn" -Network testnet
```

**功能：**
- 自动下载 FNN Windows 二进制文件
- 自动下载安装 ckb-cli Windows 版本
- 使用 PowerShell 创建账户和导出密钥
- 创建 PowerShell 和 Batch 启动脚本
- 支持双击运行（start-node.bat）

### 3. quick-start.sh - Linux/macOS 快速启动脚本

适用于已有 `fnn` 二进制文件的用户。

**使用方法：**

```bash
# 确保 fnn 二进制文件在当前目录
./quick-start.sh

# 指定安装目录和网络
./quick-start.sh /path/to/node testnet
```

## 架构支持

| 平台 | 架构 | 预编译二进制 | 源码编译 |
|------|------|-------------|---------|
| Linux | x86_64 | ✅ 可用 | ✅ |
| Linux | aarch64 (ARM64) | ✅ 可用 | ✅ |
| macOS | x86_64 | ✅ 可用 | ✅ |
| macOS | arm64 (M1/M2/M3) | ✅ 可用 | ✅ |
| Windows | x86_64 | ✅ 可用 | ✅ |

## 前置要求

### Linux/macOS

- **curl** 或 **wget**: 用于下载文件

- **ckb-cli** (可选): 用于管理 CKB 账户和密钥
  - 脚本会自动检测并提示下载安装
  - 或手动安装：
    ```bash
    # macOS
    brew install ckb-cli
    
    # Linux
    # 从 https://github.com/nervosnetwork/ckb-cli/releases 下载
    ```

- **Rust** (可选): 如果选择从源码编译需要安装
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- **unzip** (macOS 必需): 用于解压 ckb-cli
  ```bash
  brew install unzip
  ```

### Windows

- **PowerShell 5.1+** 或 **PowerShell Core 7+**
- **tar**: Windows 10/11 自带 tar 命令，用于解压 .tar.gz 文件
- 脚本会自动下载所有依赖（包括 ckb-cli）

#### Windows 使用方式

```powershell
# 在 PowerShell 中运行（可能需要调整执行策略）
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
.\install-fnn.ps1 -InstallDir "C:\my-fnn" -Network testnet
```

## 安装后使用

### Linux/macOS

安装完成后，进入安装目录：

```bash
cd my-fnn

# 使用启动脚本（会提示输入密码）
./start-node.sh

# 或手动启动
FIBER_SECRET_KEY_PASSWORD='your-password' RUST_LOG=info ./fnn -c config.yml -d .
```

### Windows

#### 方式 1：双击运行（推荐）
双击 `start-node.bat` 文件即可启动节点。

#### 方式 2：PowerShell
```powershell
cd C:\my-fnn
.\start-node.ps1
```

#### 方式 3：手动启动
```powershell
cd C:\my-fnn
$env:FIBER_SECRET_KEY_PASSWORD = 'your-password'
$env:RUST_LOG = 'info'
.\fnn.exe -c config.yml -d .
```

## 目录结构

### Linux/macOS

```
my-fnn/
├── fnn                 # FNN 二进制文件
├── config.yml          # 配置文件
├── start-node.sh       # 启动脚本
├── ckb/
│   ├── key            # 私钥文件（重要！请备份）
│   └── exported-key   # 导出的扩展私钥
└── fiber/             # 节点数据（启动后创建）
    └── store/         # 通道状态数据
```

### Windows

```
my-fnn\
├── fnn.exe             # FNN 二进制文件
├── config.yml          # 配置文件
├── start-node.ps1      # PowerShell 启动脚本
├── start-node.bat      # Batch 启动脚本（可双击运行）
├── ckb-cli.exe         # CKB CLI 工具（如自动安装）
├── ckb\
│   ├── key            # 私钥文件（重要！请备份）
│   └── exported-key   # 导出的扩展私钥
└── fiber︰             # 节点数据（启动后创建）
    └── store︰         # 通道状态数据
```

## 配置文件说明

`config.yml` 包含以下关键配置：

- **fiber.listening_addr**: P2P 监听地址
- **fiber.bootnode_addrs**: 启动节点地址
- **fiber.announced_addrs**: 对外公布的地址（如需公开节点请配置）
- **rpc.listening_addr**: RPC 监听地址
- **ckb.rpc_url**: CKB 节点 RPC 地址

## 安全提示

⚠️ **重要安全事项：**

1. **私钥安全**: `ckb/key` 文件包含您的私钥，请：
   - Linux/macOS: 设置合理的文件权限（脚本已自动设置 600）
   - Windows: 脚本已设置文件访问权限（仅当前用户可读写）
   - 定期备份到安全的地方
   - 永远不要分享或上传到公共仓库

2. **密码管理**: `FIBER_SECRET_KEY_PASSWORD` 用于加密私钥，**每次启动节点都需要此密码**。
   
   **设置密码的几种方式：**
   
   **方式 1 - 环境变量（推荐）**
   ```bash
   # Linux/macOS: 添加到 ~/.bashrc 或 ~/.zshrc
   echo 'export FIBER_SECRET_KEY_PASSWORD="your-password"' >> ~/.bashrc
   source ~/.bashrc
   
   # Windows PowerShell: 设置用户环境变量
   [Environment]::SetEnvironmentVariable("FIBER_SECRET_KEY_PASSWORD", "your-password", "User")
   ```
   
   **方式 2 - 修改启动脚本**
   ```bash
   # 编辑 start-node.sh，将 read 提示替换为：
   export FIBER_SECRET_KEY_PASSWORD="your-password"
   ```
   
   **方式 3 - 创建包装脚本**
   ```bash
   #!/bin/bash
   export FIBER_SECRET_KEY_PASSWORD="your-password"
   ./start-node.sh
   ```
   
   **密码要求：**
   - 使用强密码（至少 12 位，包含大小写字母、数字和符号）
   - 不要在不安全的环境中明文存储密码
   - Windows PowerShell 脚本使用安全输入（密码显示为星号）
   - ⚠️ **密码一旦设置无法恢复，请务必妥善保管！**

3. **防火墙**: 如果公开节点，请：
   - 配置防火墙只开放必要的端口（默认 8228）
   - RPC 端口（默认 8227）应仅限于本地访问
   - Windows: 在 Windows Defender 防火墙中添加例外时要小心

## 为账户注资

要创建支付通道和进行交易，您的节点需要有 CKB 代币。

### Testnet（测试网）

如果您运行的是测试网节点，可以免费获取测试币：

1. **CKB Testnet Faucet**: https://faucet.nervos.org/
   - 输入您的 testnet 地址（以 `ckt1` 开头）
   - 每次可领取少量测试币

2. **其他测试币来源**:
   - 查看 Nervos 官方文档获取最新的 faucet 信息
   - 关注 Nervos 社区公告

3. **建议金额**: 
   - 至少 1000+ CKB 用于测试通道创建
   - 更多测试币可进行更复杂的测试

### Mainnet（主网）

如果您运行的是主网节点，需要购买真实的 CKB：

1. **从交易所购买**: 
   - Binance、Coinbase、KuCoin 等主流交易所
   - 或者使用支持 CKB 的法币交易所

2. **提取到您的地址**:
   - 使用 mainnet 地址（以 `ckb1` 开头）
   - 建议先小额测试再转移大额资金

3. **建议金额**:
   - 通道创建: 每个通道至少 1000+ CKB
   - 交易费用: 预留少量 CKB 作为交易费
   - 根据您的业务需求决定总额

### 检查余额

```bash
# 使用 ckb-cli 查看余额
ckb-cli wallet get-capacity --lock-arg <your_lock_arg>
```

⚠️ **重要**: 安装脚本会在创建账户后提示您需要注资，请务必完成这一步再尝试打开通道！

## 升级节点

### Linux/macOS

#### 安全升级（推荐）

```bash
# 1. 先关闭所有通道（使用 RPC）
# 2. 停止节点
# 3. 备份并清理数据
cp -r fiber/store fiber/store.backup
rm -rf fiber/store
# 4. 替换 fnn 二进制文件
# 5. 重新启动
```

#### 保留通道状态升级

```bash
# 1. 停止节点
# 2. 备份数据
cp -r fiber/store fiber/store.backup
# 3. 运行迁移工具
fnn-migrate -p ./fiber/store
# 4. 替换 fnn 二进制文件
# 5. 重新启动
```

### Windows

#### 安全升级（推荐）

```powershell
# 1. 先关闭所有通道（使用 RPC）
# 2. 停止节点
# 3. 备份并清理数据
Copy-Item -Recurse fiber\store fiber\store.backup
Remove-Item -Recurse fiber\store
# 4. 替换 fnn.exe 二进制文件
# 5. 重新启动
```

#### 保留通道状态升级

```powershell
# 1. 停止节点
# 2. 备份数据
Copy-Item -Recurse fiber\store fiber\store.backup
# 3. 运行迁移工具
.\fnn-migrate.exe -p .\fiber\store
# 4. 替换 fnn.exe 二进制文件
# 5. 重新启动
```

## 故障排除

### Linux/macOS 启动失败

1. **检查 ckb-cli 安装**:
   ```bash
   ckb-cli --version
   ```

2. **检查密码是否正确**:
   ```bash
   # 确认 FIBER_SECRET_KEY_PASSWORD 环境变量已设置
   echo $FIBER_SECRET_KEY_PASSWORD
   ```

3. **检查配置文件**:
   ```bash
   # 验证配置格式
   ./fnn -c config.yml --check
   ```

### Windows 启动失败

1. **PowerShell 执行策略**:
   ```powershell
   # 如果出现执行策略错误，运行：
   Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
   ```

2. **检查 ckb-cli 安装**:
   ```powershell
   .\ckb-cli.exe --version
   # 或如果是系统安装：
   ckb-cli --version
   ```

3. **检查密码是否正确**:
   ```powershell
   # 确认 FIBER_SECRET_KEY_PASSWORD 环境变量已设置
   $env:FIBER_SECRET_KEY_PASSWORD
   ```

### 无法连接网络

1. **检查网络连接**:
   ```bash
   # 测试 CKB 节点连接
   curl https://testnet.ckbapp.dev/
   ```

2. **检查防火墙**:
   ```bash
   # 确保端口 8228 可用
   netstat -tlnp | grep 8228
   ```

3. **查看日志**:
   ```bash
   # 增加日志级别
   RUST_LOG=debug ./fnn -c config.yml -d .
   ```

### 权限问题

```bash
# 修复权限
chmod 600 ckb/key
chmod 600 ckb/exported-key
chmod +x fnn
chmod +x start-node.sh
```

## 参考资源

- [Fiber 官方文档](https://docs.fiber.world/)
- [GitHub 仓库](https://github.com/nervosnetwork/fiber)
- [RPC API 文档](https://github.com/nervosnetwork/fiber/blob/main/src/rpc/README.md)
- [CKB 文档](https://docs.nervos.org/)

## 支持

如遇问题：

1. 查看 [Fiber Issues](https://github.com/nervosnetwork/fiber/issues)
2. 加入 Nervos 社区获取帮助
3. 参考官方文档

## 许可证

这些脚本遵循与 Fiber Network 相同的许可证。
