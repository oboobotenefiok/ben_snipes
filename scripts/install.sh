#!/data/data/com.termux/files/usr/bin/bash

set -e

# Colors for output
GREEN='\033[0;32m'
PURPLE='\033[0;35m'
ORANGE='\033[0;33m'
NC='\033[0m' # No Color

# Change to Termux home directory
cd ~

echo -e "${PURPLE}╔════════════════════════════════════════════╗${NC}"
echo -e "${PURPLE}║     ben_snipes Bot Setup v0.1.0           ║${NC}"
echo -e "${PURPLE}║     by oboobotenefiok                      ║${NC}"
echo -e "${PURPLE}╚════════════════════════════════════════════╝${NC}"
echo ""

INSTALL_DIR=~

# Detect system architecture
ARCH=$(uname -m)
case $ARCH in
    aarch64)
        ARCH="arm64"
        ;;
    armv8l|armv8)
        ARCH="arm64"
        ;;
    armv7l|armv7)
        ARCH="arm"
        ;;
    x86_64)
        ARCH="amd64"
        ;;
    *)
        echo -e "${ORANGE}Unsupported architecture: $ARCH${NC}"
        echo -e "${ORANGE}Supported: aarch64 (ARMv8), armv7l (ARMv7), x86_64${NC}"
        exit 1
        ;;
esac
echo -e "${GREEN}Detected architecture: $ARCH${NC}"

# Download binary
echo -e "${GREEN}Downloading ben_snipes binary...${NC}"
REPO="oboobotenefiok/ben_snipes"
LATEST_VERSION=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep -o '"tag_name": "[^"]*"' | cut -d'"' -f4)

if [ -z "$LATEST_VERSION" ]; then
    echo -e "${ORANGE}Failed to fetch latest version${NC}"
    exit 1
fi

echo -e "${GREEN}Latest version: $LATEST_VERSION${NC}"

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_VERSION/ben_snipes-$ARCH"
CHECKSUM_URL="$DOWNLOAD_URL.sha256"

curl -L -o "$INSTALL_DIR/ben_snipes-$ARCH" "$DOWNLOAD_URL"
curl -L -o "$INSTALL_DIR/ben_snipes-$ARCH.sha256" "$CHECKSUM_URL"

# Verify checksum
echo -e "${GREEN}Verifying checksum...${NC}"
if ! (cd "$INSTALL_DIR" && sha256sum -c "ben_snipes-$ARCH.sha256"); then
    echo -e "${ORANGE}Checksum verification failed${NC}"
    exit 1
fi

# Rename and make executable
mv "$INSTALL_DIR/ben_snipes-$ARCH" "$INSTALL_DIR/ben_snipes"
rm -f "$INSTALL_DIR/ben_snipes-$ARCH.sha256"
chmod +x "$INSTALL_DIR/ben_snipes"
echo -e "${GREEN}Binary verified and ready${NC}"

# Create config and state folders
echo -e "${GREEN}Creating configuration and state directories...${NC}"
mkdir -p "$INSTALL_DIR/config"
mkdir -p "$INSTALL_DIR/state"

# Download default config
echo -e "${GREEN}Downloading default configuration...${NC}"
curl -L -o "$INSTALL_DIR/config/default.toml" \
    "https://raw.githubusercontent.com/$REPO/main/config/default.toml"
echo -e "${GREEN}Configuration downloaded to ~/config/${NC}"

# Test installation
echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${PURPLE}  Testing Installation                      ${NC}"
echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

cd ~
echo -e "${GREEN}Testing binary...${NC}"
./ben_snipes --version

echo ""
echo -e "${GREEN}Installation complete!${NC}"
echo ""
echo -e "${PURPLE}┌────────────────────────────────────────────────────────────┐${NC}"
echo -e "${PURPLE}│  NEXT STEPS                                               │${NC}"
echo -e "${PURPLE}├────────────────────────────────────────────────────────────┘${NC}"
echo -e "${ORANGE}1.${NC} The bot is installed in: ${GREEN}~${NC}"
echo -e "${ORANGE}2.${NC} Edit configuration: ${GREEN}nano ~/config/default.toml${NC}"
echo -e "   ${GREEN}- Adjust take_profit_percent${NC} (default: 10.0)"
echo -e "   ${GREEN}- Adjust max_position_size${NC} (default: 0.01 SOL)"
echo -e "   ${GREEN}- Adjust min_volume_24h${NC} (default: 250)"
echo ""
echo -e "${ORANGE}3.${NC} Set environment variables (REQUIRED for trading):"
echo -e "   ${GREEN}export SOLANA_PRIVATE_KEY=your_base58_private_key${NC}"
echo -e "   ${GREEN}export SOLANA_RPC_URL=https://api.mainnet-beta.solana.com${NC}"
echo -e "   ${GREEN}export SOLANA_PRIORITY_FEE=0.0001${NC}  # Optional"
echo ""
echo -e "${ORANGE}4.${NC} Run the bot: ${GREEN}./ben_snipes${NC}"
echo ""
echo -e "${ORANGE}5.${NC} Monitor logs: ${GREEN}RUST_LOG=info ./ben_snipes${NC}"
echo ""
echo -e "${ORANGE}6.${NC} To update: stop the bot, re-run this installer"
echo ""
echo -e "${ORANGE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${ORANGE}WARNING: IMPORTANT SAFETY NOTES${NC}"
echo -e "${ORANGE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "   ${ORANGE}* Test with detection-only mode first${NC}"
echo -e "   ${ORANGE}* Start with minimum position size (0.01 SOL)${NC}"
echo -e "   ${ORANGE}* Monitor behavior for the first hour${NC}"
echo -e "   ${ORANGE}* Never share your private key${NC}"
echo -e "   ${ORANGE}* Backup config: cp ~/config/default.toml ~/backup/${NC}"
