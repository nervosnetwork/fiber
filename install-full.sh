#!/bin/bash
# Fiber Network Node (FNN) Full Installer
# This is the full-featured installer with interactive prompts
# Usage: curl -sSfL https://get.fiber.world | bash

set -e

# ... (保留原有 install-fnn.sh 的内容) ...

# 主要修改：检测是否是管道执行
if [ ! -t 0 ]; then
    # 管道执行，使用非交互模式
    echo "Running in non-interactive mode (piped from curl)"
    echo "For interactive installation, save the script first:"
    echo "  curl -sSfL https://get.fiber.world -o install.sh"
    echo "  bash install.sh"
    echo ""
    
    # 设置默认值
    INSTALL_DIR="${INSTALL_DIR:-$HOME/.fiber}"
    NETWORK="${NETWORK:-testnet}"
    
    # 运行简化版安装
    install_fnn_simple
else
    # 交互式执行，使用完整功能
    main "$@"
fi
