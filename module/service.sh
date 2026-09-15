#!/system/bin/sh
# Boot/service entry point. Resolves keyforge.sh without trusting $0:
# content-runners (shevery service) invoke scripts as `sh -c`, where $0
# is "sh" and the module directory may be unreadable, so fall back to a
# staged copy bootstrapped from the install ZIP.
SHADOW_DIR="${KEYFORGE_SHADOW_DIR:-/data/local/tmp/keyforge}"
ZIP_DIR="${KEYFORGE_ZIP_DIR:-/sdcard/Download}"

resolve_script() {
    if [ -f "${0%/*}/keyforge.sh" ]; then
        printf '%s' "${0%/*}/keyforge.sh"
    elif [ -r "${MODDIR:-}/keyforge.sh" ]; then
        printf '%s' "${MODDIR}/keyforge.sh"
    elif [ -f "$SHADOW_DIR/keyforge.sh" ]; then
        printf '%s' "$SHADOW_DIR/keyforge.sh"
    fi
}

bootstrap_shadow() {
    [ -x "$SHADOW_DIR/keyforge" ] && [ -f "$SHADOW_DIR/keyforge.sh" ] || _need=1
    command -v unzip >/dev/null 2>&1 || return 0
    _newest=""
    for _z in "$ZIP_DIR"/*.zip; do
        [ -f "$_z" ] || continue
        unzip -p "$_z" module.prop 2>/dev/null | grep -q '^id=keyforge$' || continue
        if [ -z "$_newest" ] || [ "$_z" -nt "$_newest" ]; then _newest="$_z"; fi
    done
    [ -n "$_newest" ] || return 0
    if [ "$_need" = 1 ] || [ "$_newest" -nt "$SHADOW_DIR/keyforge" ]; then
        mkdir -p "$SHADOW_DIR"
        unzip -o "$_newest" keyforge keyforge.sh module.prop -d "$SHADOW_DIR" >/dev/null 2>&1
        chmod 755 "$SHADOW_DIR/keyforge" "$SHADOW_DIR/keyforge.sh" 2>/dev/null
    fi
}

KF="$(resolve_script)"
if [ -z "$KF" ]; then
    bootstrap_shadow
    [ -f "$SHADOW_DIR/keyforge.sh" ] && KF="$SHADOW_DIR/keyforge.sh"
fi
if [ -z "$KF" ]; then
    echo "keyforge: runtime not found (keep the module ZIP in Download)" >&2
    exit 1
fi
sh "$KF" start
