#!/data/data/com.termux/files/usr/bin/bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}╔════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║     ben_snipes Bot Setup v0.1.0           ║${NC}"
echo -e "${BLUE}║     by oboobotenefiok                      ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════╝${NC}"
echo ""

# Get the directory where the install script was called from
# This is where the bot will be run from
INSTALL_DIR="$(pwd)"
echo -e "${GREEN}Installation directory: $INSTALL_DIR${NC}"

# Detect system architecture with clear ARM version naming
ARCH=$(uname -m)
case $ARCH in
    aarch64)
        ARCH="arm64"
        ARCH_FULL="ARM64 (ARMv8)"
        ;;
    armv8l|armv8)
        ARCH="arm64"
        ARCH_FULL="ARM64 (ARMv8)"
        ;;
    armv7l|armv7)
        ARCH="arm"
        ARCH_FULL="ARMv7 (32-bit)"
        ;;
    x86_64)
        ARCH="amd64"
        ARCH_FULL="x86_64 (64-bit)"
        ;;
    *)
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        echo -e "${YELLOW}Supported architectures: aarch64 (ARMv8), armv7l (ARMv7), x86_64${NC}"
        exit 1
        ;;
esac
echo -e "${GREEN}Detected architecture: $ARCH_FULL ($ARCH)${NC}"

# 1. Download binary to the installation directory
echo -e "${GREEN}Downloading ben_snipes binary...${NC}"
REPO="oboobotenefiok/ben_snipes"
LATEST_VERSION=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep -o '"tag_name": "[^"]*"' | cut -d'"' -f4)

if [ -z "$LATEST_VERSION" ]; then
    echo -e "${RED}Failed to fetch latest version${NC}"
    exit 1
fi

echo -e "${GREEN}Latest version: $LATEST_VERSION${NC}"

# Download based on architecture
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_VERSION/ben_snipes-$ARCH"
curl -L -o "$INSTALL_DIR/ben_snipes" "$DOWNLOAD_URL"
chmod +x "$INSTALL_DIR/ben_snipes"
echo -e "${GREEN}Binary downloaded and made executable${NC}"

# 2. Create config directory in the installation location
echo -e "${GREEN}Creating configuration directory...${NC}"
mkdir -p "$INSTALL_DIR/config"
mkdir -p "$INSTALL_DIR/state"

# Also create the user config directory for backward compatibility
mkdir -p ~/.config/ben_snipes

# Download default config to both the install directory and user config
echo -e "${GREEN}Downloading default configuration...${NC}"
curl -L -o "$INSTALL_DIR/config/default.toml" \
    "https://raw.githubusercontent.com/$REPO/main/config/default.toml"
# Also keep a copy in the user config dir for the wrapper script
cp "$INSTALL_DIR/config/default.toml" ~/.config/ben_snipes/default.toml
echo -e "${GREEN}Configuration downloaded to $INSTALL_DIR/config/${NC}"

# 3. Interactive environment setup
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}  Environment Configuration                 ${NC}"
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Check for existing .env file in the install directory
ENV_FILE="$INSTALL_DIR/.env"
if [ -f "$ENV_FILE" ]; then
    echo -e "${YELLOW}Existing .env file found. Load previous values? (y/n)${NC}"
    read -r load_existing
    if [[ "$load_existing" =~ ^[Yy]$ ]]; then
        source "$ENV_FILE"
        echo -e "${GREEN}Loaded existing configuration${NC}"
    fi
fi

# Create new .env file in the install directory
echo "# ben_snipes environment configuration" > "$ENV_FILE"
echo "# Generated: $(date)" >> "$ENV_FILE"
echo "# This file should be in the same directory as the ben_snipes binary" >> "$ENV_FILE"
echo "" >> "$ENV_FILE"

# SOLANA_PRIVATE_KEY (required for trading)
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  SOLANA PRIVATE KEY                        │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  WARNING: REQUIRED for real trading       │${NC}"
echo -e "${YELLOW}│  Leave empty for detection-only mode      │${NC}"
echo -e "${YELLOW}│  Format: Base58 encoded private key       │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}Enter your Solana private key (base58):${NC}"
read -s solana_key
echo ""
if [ -n "$solana_key" ]; then
    echo "SOLANA_PRIVATE_KEY=$solana_key" >> "$ENV_FILE"
    echo -e "${GREEN}SOLANA_PRIVATE_KEY configured${NC}"
else
    echo -e "${YELLOW}No private key provided - running in detection-only mode${NC}"
fi

# RPC URL (optional, uses default if empty)
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  SOLANA RPC URL                            │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Default: https://api.mainnet-beta.solana.com${NC}"
echo -e "${YELLOW}│  Recommended: Your own RPC provider       │${NC}"
echo -e "${YELLOW}│  Examples:                                 │${NC}"
echo -e "${YELLOW}│  * Helius: https://rpc.helius.xyz/YOUR-KEY${NC}"
echo -e "${YELLOW}│  * QuickNode: https://YOUR-ENDPOINT.quicknode.com${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}Enter RPC URL (press Enter for default):${NC}"
read -r rpc_url
if [ -n "$rpc_url" ]; then
    echo "SOLANA_RPC_URL=$rpc_url" >> "$ENV_FILE"
    echo -e "${GREEN}Custom RPC URL configured${NC}"
else
    echo "SOLANA_RPC_URL=https://api.mainnet-beta.solana.com" >> "$ENV_FILE"
    echo -e "${YELLOW}Using default RPC URL (may be rate-limited)${NC}"
fi

# Optional: Priority fee
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  PRIORITY FEE (Optional)                   │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Default: 0.0001 SOL                      │${NC}"
echo -e "${YELLOW}│  Higher fee = faster confirmation        │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}Enter priority fee in SOL (press Enter for default):${NC}"
read -r priority_fee
if [ -n "$priority_fee" ]; then
    echo "SOLANA_PRIORITY_FEE=$priority_fee" >> "$ENV_FILE"
    echo -e "${GREEN}Priority fee set: $priority_fee SOL${NC}"
fi

# Optional: EVM chains
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  EVM CHAINS (Optional)                     │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Would you like to configure EVM chains?  │${NC}"
echo -e "${YELLOW}│  This requires additional configuration  │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}Configure EVM chains now? (y/N)${NC}"
read -r configure_evm
if [[ "$configure_evm" =~ ^[Yy]$ ]]; then
    # Update the config in the install directory
    CONFIG_FILE="$INSTALL_DIR/config/default.toml"
    echo "" >> "$CONFIG_FILE"
    echo "# EVM Chain Configuration" >> "$CONFIG_FILE"
    echo "# Uncomment and configure as needed" >> "$CONFIG_FILE"
    echo "# See: https://github.com/$REPO#evm-configuration" >> "$CONFIG_FILE"

    echo -e "${BLUE}Enter chain name (e.g., ethereum, base):${NC}"
    read -r chain_name
    echo -e "${BLUE}Enter WebSocket RPC URL (with API key):${NC}"
    read -r ws_rpc
    echo -e "${BLUE}Enter factory contract address:${NC}"
    read -r factory_addr
    echo -e "${BLUE}Enter topic0 hash:${NC}"
    read -r topic0
    echo -e "${BLUE}Enter base asset addresses (comma-separated):${NC}"
    read -r base_assets

    if [ -n "$chain_name" ] && [ -n "$ws_rpc" ] && [ -n "$factory_addr" ] && [ -n "$topic0" ]; then
        echo "" >> "$CONFIG_FILE"
        echo "[[evm_chains]]" >> "$CONFIG_FILE"
        echo "chain_name = \"$chain_name\"" >> "$CONFIG_FILE"
        echo "ws_rpc_url = \"$ws_rpc\"" >> "$CONFIG_FILE"
        echo "factory_address = \"$factory_addr\"" >> "$CONFIG_FILE"
        echo "topic0 = \"$topic0\"" >> "$CONFIG_FILE"

        if [ -n "$base_assets" ]; then
            IFS=',' read -ra ADDR <<< "$base_assets"
            echo -n "base_assets = [" >> "$CONFIG_FILE"
            for i in "${!ADDR[@]}"; do
                if [ $i -eq $((${#ADDR[@]} - 1)) ]; then
                    echo "\"${ADDR[$i]}\"" >> "$CONFIG_FILE"
                else
                    echo -n "\"${ADDR[$i]}\", " >> "$CONFIG_FILE"
                fi
            done
            echo "]" >> "$CONFIG_FILE"
        fi
        echo -e "${GREEN}EVM chain configured: $chain_name${NC}"

        # Also update the user config for the wrapper
        cp "$CONFIG_FILE" ~/.config/ben_snipes/default.toml
    fi
fi

# Export current session from the .env file
set -a
source "$ENV_FILE"
set +a

# 4. Create wrapper script in the installation directory
echo -e "${GREEN}Creating wrapper script...${NC}"
cat > "$INSTALL_DIR/ben_snipes-wrapper" << 'EOF'
#!/data/data/com.termux/files/usr/bin/bash

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Load environment variables from the .env file in the same directory
if [ -f "$SCRIPT_DIR/.env" ]; then
    set -a
    source "$SCRIPT_DIR/.env"
    set +a
fi

# Run the bot from its own directory
cd "$SCRIPT_DIR"
exec ./ben_snipes "$@"
EOF
chmod +x "$INSTALL_DIR/ben_snipes-wrapper"
echo -e "${GREEN}Wrapper script created${NC}"

# Also create a symlink in ~/.local/bin for easy access (optional)
mkdir -p ~/.local/bin
ln -sf "$INSTALL_DIR/ben_snipes-wrapper" ~/.local/bin/ben_snipes 2>/dev/null || true

# 5. Test the installation
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}  Test Installation                         ${NC}"
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

cd "$INSTALL_DIR"
echo -e "${GREEN}Testing binary...${NC}"
./ben_snipes --version 2>/dev/null || echo -e "${YELLOW}Version check skipped (--version not implemented)${NC}"

echo ""
echo -e "${GREEN}Installation complete!${NC}"
echo ""
echo -e "${BLUE}┌────────────────────────────────────────────────────────────┐${NC}"
echo -e "${BLUE}│  NEXT STEPS                                               │${NC}"
echo -e "${BLUE}├────────────────────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}1.${NC} The bot is installed in: ${BLUE}$INSTALL_DIR${NC}"
echo -e "${YELLOW}2.${NC} Edit configuration: ${BLUE}nano $INSTALL_DIR/config/default.toml${NC}"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}take_profit_percent${NC} (default: 10.0)"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}max_position_size${NC} (default: 0.01 SOL)"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}min_volume_24h${NC} (default: 250)"
echo ""
echo -e "${YELLOW}3.${NC} Run the bot: ${BLUE}cd $INSTALL_DIR && ./ben_snipes-wrapper${NC}"
echo -e "${YELLOW}   ${NC}Or from anywhere: ${BLUE}ben_snipes${NC} (if ~/.local/bin is in PATH)"
echo ""
echo -e "${YELLOW}4.${NC} To update: ${BLUE}./ben_snipes-wrapper --update${NC}"
echo ""
echo -e "${YELLOW}5.${NC} Monitor logs: ${BLUE}RUST_LOG=info ./ben_snipes-wrapper${NC}"
echo ""
echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${RED}WARNING: IMPORTANT SAFETY NOTES${NC}"
echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}   * ${RED}Verify solana-sdk compatibility before trading${NC}"
echo -e "${YELLOW}   * ${RED}Test with detection-only mode first${NC}"
echo -e "${YELLOW}   * ${RED}Start with the minimum position size (0.01 SOL)${NC}"
echo -e "${YELLOW}   * ${RED}Monitor the bot's behavior for the first hour${NC}"
echo -e "${YELLOW}   * ${RED}Never share your private key${NC}"
echo -e "${YELLOW}   * ${RED}Backup your configuration: cp $INSTALL_DIR/.env ~/backup/${NC}"
echo ""
