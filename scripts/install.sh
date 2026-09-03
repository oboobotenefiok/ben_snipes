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

# Detect system architecture
ARCH=$(uname -m)
case $ARCH in
    aarch64) ARCH="arm64" ;;
    armv7l) ARCH="arm" ;;
    x86_64) ARCH="amd64" ;;
    *) echo -e "${RED}Unsupported architecture: $ARCH${NC}"; exit 1 ;;
esac
echo -e "${GREEN}✓ Detected architecture: $ARCH${NC}"

# 1. Download binary
echo -e "${GREEN}→ Downloading ben_snipes binary...${NC}"
REPO="oboobotenefiok/ben_snipes"
LATEST_VERSION=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep -o '"tag_name": "[^"]*"' | cut -d'"' -f4)

if [ -z "$LATEST_VERSION" ]; then
    echo -e "${RED}Failed to fetch latest version${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Latest version: $LATEST_VERSION${NC}"

# Download based on architecture
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_VERSION/ben_snipes-$ARCH"
curl -L -o ben_snipes "$DOWNLOAD_URL"
chmod +x ben_snipes
echo -e "${GREEN}✓ Binary downloaded and made executable${NC}"

# 2. Create config directory
echo -e "${GREEN}→ Creating configuration directory...${NC}"
mkdir -p ~/.config/ben_snipes

# Download default config from repository
echo -e "${GREEN}→ Downloading default configuration...${NC}"
curl -L -o ~/.config/ben_snipes/default.toml \
    "https://raw.githubusercontent.com/$REPO/main/config/default.toml"
echo -e "${GREEN}✓ Configuration downloaded${NC}"

# 3. Interactive environment setup
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}  Environment Configuration                 ${NC}"
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Check for existing .env file
ENV_FILE="$HOME/.config/ben_snipes/.env"
if [ -f "$ENV_FILE" ]; then
    echo -e "${YELLOW}Existing .env file found. Load previous values? (y/n)${NC}"
    read -r load_existing
    if [[ "$load_existing" =~ ^[Yy]$ ]]; then
        source "$ENV_FILE"
        echo -e "${GREEN}✓ Loaded existing configuration${NC}"
    fi
fi

# Create new .env file
echo "# ben_snipes environment configuration" > "$ENV_FILE"
echo "# Generated: $(date)" >> "$ENV_FILE"
echo "" >> "$ENV_FILE"

# SOLANA_PRIVATE_KEY (required for trading)
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  SOLANA PRIVATE KEY                        │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  ⚠️  REQUIRED for real trading             │${NC}"
echo -e "${YELLOW}│  Leave empty for detection-only mode      │${NC}"
echo -e "${YELLOW}│  Format: Base58 encoded private key       │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}→ Enter your Solana private key (base58):${NC}"
read -s solana_key
echo ""
if [ -n "$solana_key" ]; then
    echo "SOLANA_PRIVATE_KEY=$solana_key" >> "$ENV_FILE"
    echo -e "${GREEN}✓ SOLANA_PRIVATE_KEY configured${NC}"
else
    echo -e "${YELLOW}⚠ No private key provided - running in detection-only mode${NC}"
fi

# RPC URL (optional, uses default if empty)
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  SOLANA RPC URL                            │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Default: https://api.mainnet-beta.solana.com${NC}"
echo -e "${YELLOW}│  Recommended: Your own RPC provider       │${NC}"
echo -e "${YELLOW}│  Examples:                                 │${NC}"
echo -e "${YELLOW}│  • Helius: https://rpc.helius.xyz/YOUR-KEY${NC}"
echo -e "${YELLOW}│  • QuickNode: https://YOUR-ENDPOINT.quicknode.com${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}→ Enter RPC URL (press Enter for default):${NC}"
read -r rpc_url
if [ -n "$rpc_url" ]; then
    echo "SOLANA_RPC_URL=$rpc_url" >> "$ENV_FILE"
    echo -e "${GREEN}✓ Custom RPC URL configured${NC}"
else
    echo "SOLANA_RPC_URL=https://api.mainnet-beta.solana.com" >> "$ENV_FILE"
    echo -e "${YELLOW}⚠ Using default RPC URL (may be rate-limited)${NC}"
fi

# Optional: Priority fee
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  PRIORITY FEE (Optional)                   │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Default: 0.0001 SOL                      │${NC}"
echo -e "${YELLOW}│  Higher fee = faster confirmation        │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}→ Enter priority fee in SOL (press Enter for default):${NC}"
read -r priority_fee
if [ -n "$priority_fee" ]; then
    echo "SOLANA_PRIORITY_FEE=$priority_fee" >> "$ENV_FILE"
    echo -e "${GREEN}✓ Priority fee set: $priority_fee SOL${NC}"
fi

# Optional: EVM chains
echo -e "${YELLOW}┌────────────────────────────────────────────┐${NC}"
echo -e "${YELLOW}│  EVM CHAINS (Optional)                     │${NC}"
echo -e "${YELLOW}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}│  Would you like to configure EVM chains?  │${NC}"
echo -e "${YELLOW}│  This requires additional configuration  │${NC}"
echo -e "${YELLOW}└────────────────────────────────────────────┘${NC}"
echo -e "${BLUE}→ Configure EVM chains now? (y/N)${NC}"
read -r configure_evm
if [[ "$configure_evm" =~ ^[Yy]$ ]]; then
    echo "" >> ~/.config/ben_snipes/default.toml
    echo "# EVM Chain Configuration" >> ~/.config/ben_snipes/default.toml
    echo "# Uncomment and configure as needed" >> ~/.config/ben_snipes/default.toml
    echo "# See: https://github.com/$REPO#evm-configuration" >> ~/.config/ben_snipes/default.toml
    
    echo -e "${BLUE}→ Enter chain name (e.g., ethereum, base):${NC}"
    read -r chain_name
    echo -e "${BLUE}→ Enter WebSocket RPC URL (with API key):${NC}"
    read -r ws_rpc
    echo -e "${BLUE}→ Enter factory contract address:${NC}"
    read -r factory_addr
    echo -e "${BLUE}→ Enter topic0 hash:${NC}"
    read -r topic0
    echo -e "${BLUE}→ Enter base asset addresses (comma-separated):${NC}"
    read -r base_assets
    
    if [ -n "$chain_name" ] && [ -n "$ws_rpc" ] && [ -n "$factory_addr" ] && [ -n "$topic0" ]; then
        echo "" >> ~/.config/ben_snipes/default.toml
        echo "[[evm_chains]]" >> ~/.config/ben_snipes/default.toml
        echo "chain_name = \"$chain_name\"" >> ~/.config/ben_snipes/default.toml
        echo "ws_rpc_url = \"$ws_rpc\"" >> ~/.config/ben_snipes/default.toml
        echo "factory_address = \"$factory_addr\"" >> ~/.config/ben_snipes/default.toml
        echo "topic0 = \"$topic0\"" >> ~/.config/ben_snipes/default.toml
        
        if [ -n "$base_assets" ]; then
            IFS=',' read -ra ADDR <<< "$base_assets"
            echo -n "base_assets = [" >> ~/.config/ben_snipes/default.toml
            for i in "${!ADDR[@]}"; do
                if [ $i -eq $((${#ADDR[@]} - 1)) ]; then
                    echo "\"${ADDR[$i]}\"" >> ~/.config/ben_snipes/default.toml
                else
                    echo -n "\"${ADDR[$i]}\", " >> ~/.config/ben_snipes/default.toml
                fi
            done
            echo "]" >> ~/.config/ben_snipes/default.toml
        fi
        echo -e "${GREEN}✓ EVM chain configured: $chain_name${NC}"
    fi
fi

# Add environment variables to termux profile
echo -e "${GREEN}→ Adding environment variables to Termux profile...${NC}"
BASHRC="$HOME/.bashrc"

# Remove existing ben_snipes entries if any
if grep -q "# ben_snipes environment" "$BASHRC" 2>/dev/null; then
    sed -i '/# ben_snipes environment/,/fi/d' "$BASHRC"
fi

# Add new entries
echo "" >> "$BASHRC"
echo "# ben_snipes environment" >> "$BASHRC"
echo "if [ -f ~/.config/ben_snipes/.env ]; then" >> "$BASHRC"
echo "    set -a" >> "$BASHRC"
echo "    source ~/.config/ben_snipes/.env" >> "$BASHRC"
echo "    set +a" >> "$BASHRC"
echo "fi" >> "$BASHRC"
echo -e "${GREEN}✓ Added to ~/.bashrc${NC}"

# Export current session
set -a
source "$ENV_FILE"
set +a

# 4. Create wrapper script
echo -e "${GREEN}→ Creating wrapper script...${NC}"
cat > ben_snipes-wrapper << 'EOF'
#!/data/data/com.termux/files/usr/bin/bash
if [ -f ~/.config/ben_snipes/.env ]; then
    set -a
    source ~/.config/ben_snipes/.env
    set +a
fi
exec ./ben_snipes "$@"
EOF
chmod +x ben_snipes-wrapper
echo -e "${GREEN}✓ Wrapper script created${NC}"

# 5. Test the installation
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}  Test Installation                         ${NC}"
echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

echo -e "${GREEN}→ Testing binary...${NC}"
./ben_snipes --version 2>/dev/null || echo -e "${YELLOW}⚠ Version check skipped (--version not implemented)${NC}"

echo ""
echo -e "${GREEN}✅ Installation complete!${NC}"
echo ""
echo -e "${BLUE}┌────────────────────────────────────────────┐${NC}"
echo -e "${BLUE}│  NEXT STEPS                               │${NC}"
echo -e "${BLUE}├────────────────────────────────────────────┘${NC}"
echo -e "${YELLOW}1.${NC} Edit configuration: ${BLUE}nano ~/.config/ben_snipes/default.toml${NC}"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}take_profit_percent${NC} (default: 10.0)"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}max_position_size${NC} (default: 0.01 SOL)"
echo -e "${YELLOW}   ${NC}- Adjust ${BLUE}min_volume_24h${NC} (default: 250)"
echo ""
echo -e "${YELLOW}2.${NC} Run the bot: ${BLUE}./ben_snipes-wrapper${NC}"
echo -e "${YELLOW}   ${NC}Or directly: ${BLUE}./ben_snipes${NC}"
echo ""
echo -e "${YELLOW}3.${NC} To update: ${BLUE}./ben_snipes-wrapper --update${NC}"
echo ""
echo -e "${YELLOW}4.${NC} Monitor logs: ${BLUE}RUST_LOG=info ./ben_snipes-wrapper${NC}"
echo ""
echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${RED}⚠  IMPORTANT SAFETY NOTES${NC}"
echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${YELLOW}   • ${RED}Verify solana-sdk compatibility before trading${NC}"
echo -e "${YELLOW}   • ${RED}Test with detection-only mode first${NC}"
echo -e "${YELLOW}   • ${RED}Start with the minimum position size (0.01 SOL)${NC}"
echo -e "${YELLOW}   • ${RED}Monitor the bot's behavior for the first hour${NC}"
echo -e "${YELLOW}   • ${RED}Never share your private key${NC}"
echo -e "${YELLOW}   • ${RED}Backup your configuration: cp ~/.config/ben_snipes/.env ~/backup/${NC}"
echo ""
