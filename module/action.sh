#!/system/bin/sh
# Manual action entrypoint (Shizuku action card, KernelSU action button).
# Prints daemon status; never touches device state.

MODDIR="${0%/*}"

echo "module=keyforge"
sh "$MODDIR/keyforge.sh" status 2>&1
sh "$MODDIR/keyforge.sh" runtime 2>&1
