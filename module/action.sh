#!/system/bin/sh
# Manual action entrypoint (Shizuku action card, KernelSU action button).
# Prints daemon status; never touches device state.

SHADOW_DIR="${KEYFORGE_SHADOW_DIR:-/data/local/tmp/keyforge}"

_self="${0%/*}"
if [ -f "$_self/keyforge.sh" ]; then
    KF="$_self/keyforge.sh"
elif [ -r "${MODDIR:-}/keyforge.sh" ]; then
    KF="${MODDIR}/keyforge.sh"
elif [ -f "$SHADOW_DIR/keyforge.sh" ]; then
    KF="$SHADOW_DIR/keyforge.sh"
else
    echo "keyforge: runtime not found" >&2
    exit 1
fi

echo "module=keyforge"
sh "$KF" status 2>&1
sh "$KF" runtime 2>&1
