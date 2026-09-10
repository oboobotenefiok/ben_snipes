#!/data/data/com.termux/files/usr/bin/bash

set -euo pipefail

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

# Test installation.
#
# --version must be handled before any config, network, or wallet access
# in main.rs, and must exit immediately. If this line ever hangs, the
# binary predates that fix and will launch the real bot here - including
# real RPC connections and, if SOLANA_PRIVATE_KEY happens to be set in
# the environment, real trading.
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
echo -e "${PURPLE}│  NEXT STEPS                                                │${NC}"
echo -e "${PURPLE}├────────────────────────────────────────────────────────────┘${NC}"
echo -e "${ORANGE}1.${NC} The bot is installed in: ${GREEN}~${NC}"
echo ""
echo -e "${ORANGE}2.${NC} Edit configuration: ${GREEN}nano ~/config/default.toml${NC}"
echo -e "   ${GREEN}- take_profit_percent${NC} (default: 10.0)"
echo -e "   ${GREEN}- max_position_size${NC}   (default: 0.01 SOL)"
echo -e "   ${GREEN}- min_volume_24h${NC}      (default: 1)"
echo ""
echo -e "${ORANGE}3.${NC} Set environment variables (REQUIRED for live trading):"
echo ""
echo -e "   ${ORANGE}a)${NC} Wallet key - read directly, NOT prefixed:"
echo -e "      ${GREEN}export SOLANA_PRIVATE_KEY=\"your_base58_or_hex_key\"${NC}"
echo -e "      ${ORANGE}WARNING: must be a Solana (ed25519) key, not an EVM key.${NC}"
echo -e "      ${ORANGE}A key from the wrong curve will load silently and control nothing.${NC}"
echo ""
echo -e "   ${ORANGE}b)${NC} RPC endpoints - these use the BEN_SNIPES__ prefix:"
echo -e "      ${GREEN}export BEN_SNIPES__SOLANA__READ_RPC_URL=\"https://your-provider/...\"${NC}"
echo -e "      ${GREEN}export BEN_SNIPES__SOLANA__WRITE_RPC_URL=\"https://your-provider/...\"${NC}"
echo -e "      ${ORANGE}Note the DOUBLE underscore after BEN_SNIPES and between SOLANA and the field name.${NC}"
echo -e "      ${ORANGE}A single underscore there is silently ignored - the bot falls back to the${NC}"
echo -e "      ${ORANGE}public api.mainnet-beta endpoint, which is rate-limited and unsuitable for trading.${NC}"
echo ""
echo -e "   ${ORANGE}c)${NC} Optional overrides (same BEN_SNIPES__ prefix):"
echo -e "      ${GREEN}export BEN_SNIPES__SOLANA__CONFIRM_RPC_URL=\"...\"${NC}  # defaults to WRITE_RPC_URL"
echo -e "      ${GREEN}export BEN_SNIPES__SOLANA__PRIORITY_FEE_SOL=\"0.0001\"${NC}"
echo -e "      ${GREEN}export BEN_SNIPES__SOLANA__SLIPPAGE_PERCENT=\"10\"${NC}"
echo -e "      ${GREEN}export BEN_SNIPES__RISK__MAX_POSITION_SIZE=\"0.01\"${NC}"
echo ""
echo -e "   ${ORANGE}Verify before running:${NC} ${GREEN}env | grep BEN_SNIPES${NC}"
echo -e "      You should see exactly the variables above and nothing else."
echo ""
echo -e "${ORANGE}4.${NC} Run the bot: ${GREEN}./ben_snipes${NC}"
echo ""
echo -e "${ORANGE}5.${NC} Monitor logs: ${GREEN}RUST_LOG=info ./ben_snipes${NC}"
echo ""
echo -e "${ORANGE}6.${NC} Check the startup log line:"
echo -e "   ${GREEN}read_rpc_url=https://...  write_rpc_url=https://...${NC}"
echo -e "   Confirm these show YOUR provider, not ${ORANGE}api.mainnet-beta.solana.com${NC}."
echo -e "   ${ORANGE}If they still show the public endpoint, the env var name is wrong.${NC}"
echo ""
echo -e "${ORANGE}7.${NC} To update: stop the bot, re-run this installer."
echo ""
echo -e "${ORANGE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${ORANGE}WARNING: IMPORTANT SAFETY NOTES${NC}"
echo -e "${ORANGE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "   ${ORANGE}* Live mode signs and broadcasts real transactions. There is no dry-run flag.${NC}"
echo -e "   ${ORANGE}* Start with the default 0.01 SOL position size.${NC}"
echo -e "   ${ORANGE}* Fund the wallet with only what you are willing to lose.${NC}"
echo -e "   ${ORANGE}* Test on a throwaway wallet first.${NC}"
echo -e "   ${ORANGE}* Never share your private key. Never paste it into a chat or issue tracker.${NC}"
echo -e "   ${ORANGE}* Back up config: cp ~/config/default.toml ~/backup/${NC}"
echo -e "   ${ORANGE}* To halt new entries without stopping the bot:${NC}"
echo -e "     ${GREEN}touch ~/state/STOP_ENTRIES${NC}  (remove the file to resume)"
