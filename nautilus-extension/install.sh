#!/bin/bash
# Install davfs-sync Nautilus extension for current user

set -e

echo "Installing davfs-sync Nautilus extension..."

# Check if nautilus-python is installed
if ! python3 -c "import gi; gi.require_version('Nautilus', '4.1'); from gi.repository import Nautilus" 2>/dev/null; then
    echo "Error: nautilus-python is not installed"
    echo ""
    echo "Install it with:"
    echo "  Fedora/RHEL: sudo dnf install nautilus-python"
    echo "  Ubuntu/Debian: sudo apt install python3-nautilus"
    exit 1
fi

# Create extensions directory
EXTENSIONS_DIR="$HOME/.local/share/nautilus-python/extensions"
mkdir -p "$EXTENSIONS_DIR"

# Copy extension
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cp "$SCRIPT_DIR/davfs_sync_nautilus.py" "$EXTENSIONS_DIR/"

echo "✓ Extension installed to: $EXTENSIONS_DIR/davfs_sync_nautilus.py"

# Make it executable
chmod +x "$EXTENSIONS_DIR/davfs_sync_nautilus.py"

echo ""
echo "Restarting Nautilus..."
nautilus -q 2>/dev/null || true

sleep 1

echo "✓ Installation complete!"
echo ""
echo "The extension will show sync status emblems in Nautilus for"
echo "files in davfs-sync mounted directories."
echo ""
echo "If emblems don't appear, try:"
echo "  1. Close all Nautilus windows"
echo "  2. Run: nautilus -q"
echo "  3. Open a davfs-sync mount point in Nautilus"
