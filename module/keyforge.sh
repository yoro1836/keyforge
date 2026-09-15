#!/system/bin/sh
# KeyForge control script for WebUI and module management.

SHADOW_DIR="${KEYFORGE_SHADOW_DIR:-/data/local/tmp/keyforge}"

# Resolve the runtime home. Content-runners (shevery service/action) invoke
# scripts as `sh -c`, where $0 is "sh", so prefer the script's own directory,
# then the staged copy, then the environment.
resolve_moddir() {
    if [ -f "${0%/*}/keyforge.sh" ]; then
        printf '%s' "${0%/*}"
    elif [ -f "$SHADOW_DIR/keyforge.sh" ]; then
        printf '%s' "$SHADOW_DIR"
    else
        printf '%s' "${MODDIR:-${0%/*}}"
    fi
}
MODDIR="$(resolve_moddir)"
BIN="$MODDIR/keyforge"
CONF="$MODDIR/keyforge.conf"
PIDFILE="$MODDIR/keyforge.pid"
LOG="$MODDIR/keyforge.log"
HIDDEN_STATE="$MODDIR/.hidden-device.json"
RUNTIME_DIR_FILE="$MODDIR/.runtime-dir"
DATA_DIR="/sdcard/.keyforge"
MANIFEST="$DATA_DIR/manifest.json"
PLUGIN_DIR="$DATA_DIR/plugins"

log() {
    echo "[keyforge] $(date '+%H:%M:%S') $*" >> "$LOG"
}

runtime_dir() {
    _saved=""
    [ -f "$RUNTIME_DIR_FILE" ] && read -r _saved < "$RUNTIME_DIR_FILE" 2>/dev/null
    for _dir in "${KEYFORGE_RUNTIME_DIR:-}" "$_saved" "${TMPDIR:-}" /data/local/tmp /dev "$MODDIR"; do
        [ -n "$_dir" ] && [ -d "$_dir" ] || continue
        _probe="$_dir/.keyforge-tmp-$$"
        if : > "$_probe" 2>/dev/null; then
            rm -f "$_probe"
            printf '%s' "$_dir"
            return 0
        fi
    done
    printf '%s' "$MODDIR"
}

is_axmanager_module() {
    [ "${AXERON:-}" = "true" ] && return 0
    case "$MODDIR" in
        /data/user_de/0/com.android.shell/axeron/plugins/*|\
        /data/user_de/0/android/axeron/plugins/*) return 0 ;;
        *) return 1 ;;
    esac
}

detect_framework() {
    if is_axmanager_module; then
        echo "axmanager"
    elif [ "${KSU:-}" = "true" ] || [ -x /data/adb/ksu/bin/busybox ]; then
        echo "kernelsu"
    elif [ -n "${MAGISK_VER_CODE:-}" ] || [ -x /data/adb/magisk/busybox ]; then
        echo "magisk"
    elif [ -n "${SHIZUKU_MODULE_ID:-}" ]; then
        echo "shizuku"
    elif [ -d /data/user_de/0/com.android.shell/axeron ] || \
         [ -d /data/user_de/0/android/axeron ]; then
        echo "axmanager"
    else
        echo "unknown"
    fi
}

supports_device_hide() {
    case "$(detect_framework)" in
        kernelsu|magisk) return 0 ;;
        # Shizuku runs scripts as ADB shell or root; hiding needs root.
        shizuku) [ "$(id -u 2>/dev/null)" = "0" ] ;;
        *) return 1 ;;
    esac
}

is_running() {
    [ -f "$PIDFILE" ] || return 1
    _pid=""
    read -r _pid < "$PIDFILE" 2>/dev/null || return 1
    [ -n "$_pid" ] && [ -r "/proc/$_pid/cmdline" ] || {
        rm -f "$PIDFILE"
        return 1
    }
    _cmd="$(tr '\000' ' ' < "/proc/$_pid/cmdline" 2>/dev/null)"
    case "$_cmd" in
        *"$BIN"*) return 0 ;;
    esac
    rm -f "$PIDFILE"
    return 1
}

valid_key() {
    case "${1:-}" in
        ""|*[!A-Za-z0-9._-]*) return 1 ;;
        *) return 0 ;;
    esac
}

config_get() {
    _key="${1:-}"
    valid_key "$_key" || return 2
    [ -f "$CONF" ] || return 1
    awk -v key="$_key" '
        {
            pos = index($0, "=")
            if (pos > 0 && tolower(substr($0, 1, pos - 1)) == tolower(key)) {
                print substr($0, pos + 1)
                exit
            }
        }
    ' "$CONF"
}

config_set() {
    _key="${1:-}"
    _value="${2:-}"
    valid_key "$_key" || {
        echo "keyforge: invalid configuration key" >&2
        return 2
    }
    case "$_value" in
        *"
"*) echo "keyforge: configuration values must be one line" >&2; return 2 ;;
    esac
    ensure_conf
    _tmp="$CONF.tmp.$$"
    awk -v key="$_key" -v value="$_value" '
        BEGIN { found = 0 }
        {
            pos = index($0, "=")
            current = pos > 0 ? substr($0, 1, pos - 1) : ""
            if (pos > 0 && tolower(current) == tolower(key)) {
                if (!found) print key "=" value
                found = 1
            } else {
                print
            }
        }
        END { if (!found) print key "=" value }
    ' "$CONF" > "$_tmp" && mv -f "$_tmp" "$CONF"
    _result=$?
    [ "$_result" -eq 0 ] || rm -f "$_tmp"
    return "$_result"
}

ensure_conf() {
    if [ ! -f "$CONF" ]; then
        cat > "$CONF" << EOF
# keyforge configuration
VID=0x045e
PID=0x028e
PLUGIN_DIR=/sdcard/.keyforge/plugins
HIDE_DEVICE=0
EOF
    elif ! grep -qi '^HIDE_DEVICE=' "$CONF" 2>/dev/null; then
        echo "HIDE_DEVICE=0" >> "$CONF"
    fi
}

restore_hidden_device() {
    [ -x "$BIN" ] || return 0
    [ -f "$HIDDEN_STATE" ] || return 0
    "$BIN" --restore-hidden-state "$HIDDEN_STATE" >> "$LOG" 2>&1
}

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/	/\\t/g'
}

emit_device() {
    [ -n "$_handler" ] && [ -n "$_name" ] || return 0
    [ "$_name" = "KeyForge Virtual Controller" ] && return 0
    [ "$_first" = "1" ] && _first=0 || printf ','
    printf '{"name":"%s","vid":"%s","pid":"%s","handler":"%s"}' \
        "$(json_escape "$_name")" "${_vid:-0x0000}" "${_pid:-0x0000}" \
        "$(json_escape "$_handler")"
}

case "${1:-}" in
    start)
        ensure_conf
        mkdir -p "$MODDIR/plugins" "$PLUGIN_DIR" "$DATA_DIR/configs" 2>/dev/null
        KEYFORGE_RUNTIME_DIR="$(runtime_dir)"
        export KEYFORGE_RUNTIME_DIR
        printf '%s\n' "$KEYFORGE_RUNTIME_DIR" > "$RUNTIME_DIR_FILE"
        chmod 600 "$RUNTIME_DIR_FILE" 2>/dev/null || :
        if is_running; then
            echo "keyforge: already running (pid $(cat "$PIDFILE"))"
            exit 0
        fi
        if [ ! -f "$BIN" ]; then
            echo "keyforge: FATAL - binary not found: $BIN"
            exit 1
        fi
        chmod 755 "$BIN" 2>/dev/null || :
        restore_hidden_device
        log "starting daemon on $(detect_framework)"
        if supports_device_hide; then
            "$BIN" --config "$CONF" --hidden-state "$HIDDEN_STATE" \
                --allow-device-hide --pidfile "$PIDFILE" >> "$LOG" 2>&1 < /dev/null
        else
            "$BIN" --config "$CONF" --hidden-state "$HIDDEN_STATE" \
                --pidfile "$PIDFILE" >> "$LOG" 2>&1 < /dev/null
        fi
        # The daemon double-forks and writes its own pidfile; the call above
        # returns as soon as the first child detaches.
        _wait=0
        while [ "$_wait" -lt 15 ]; do
            is_running && break
            sleep 0.1
            _wait=$((_wait + 1))
        done
        if is_running; then
            log "started (pid=$(cat "$PIDFILE"))"
            echo "keyforge: started (pid=$(cat "$PIDFILE"))"
        else
            restore_hidden_device
            log "died"
            echo "keyforge: FAILED - see $LOG"
            exit 1
        fi
        ;;

    stop)
        if is_running; then
            _pid=""
            read -r _pid < "$PIDFILE" 2>/dev/null
            log "stopping (pid=$_pid)"
            kill "$_pid" 2>/dev/null || :
            _wait=0
            while [ -d "/proc/$_pid" ] && [ "$_wait" -lt 20 ]; do
                sleep 0.1
                _wait=$((_wait + 1))
            done
            [ -d "/proc/$_pid" ] && kill -9 "$_pid" 2>/dev/null || :
            rm -f "$PIDFILE"
            restore_hidden_device
            echo "keyforge: stopped"
        else
            restore_hidden_device
            echo "keyforge: not running"
        fi
        ;;

    restart)
        "$0" stop
        sleep 0.5
        "$0" start
        ;;

    status)
        if is_running; then
            echo "running pid=$(cat "$PIDFILE")"
        else
            echo "stopped"
        fi
        ;;

    runtime)
        ensure_conf
        _framework="$(detect_framework)"
        _supported=false
        supports_device_hide && _supported=true
        _enabled=false
        [ "$(config_get HIDE_DEVICE)" = "1" ] && _enabled=true
        _active=false
        [ -f "$HIDDEN_STATE" ] && _active=true
        printf '{"framework":"%s","deviceHideSupported":%s,"deviceHideEnabled":%s,"deviceHideActive":%s}\n' \
            "$_framework" "$_supported" "$_enabled" "$_active"
        ;;

    hide)
        ensure_conf
        case "${2:-status}" in
            on)
                if ! supports_device_hide; then
                    echo "keyforge: device hiding requires root (KernelSU, Magisk, or root shevery)" >&2
                    exit 2
                fi
                config_set HIDE_DEVICE 1 || exit $?
                echo "keyforge: physical device hiding enabled"
                ;;
            off)
                config_set HIDE_DEVICE 0 || exit $?
                is_running || restore_hidden_device
                echo "keyforge: physical device hiding disabled"
                ;;
            status)
                printf 'framework=%s supported=%s enabled=%s\n' \
                    "$(detect_framework)" \
                    "$(supports_device_hide && echo 1 || echo 0)" \
                    "$([ "$(config_get HIDE_DEVICE)" = "1" ] && echo 1 || echo 0)"
                ;;
            *) echo "usage: keyforge.sh hide {on|off|status}" >&2; exit 2 ;;
        esac
        ;;

    manifest)
        if [ -f "$MANIFEST" ]; then
            cat "$MANIFEST"
        else
            echo '{"plugins":[]}'
        fi
        ;;

    config)
        ensure_conf
        case "${2:-}" in
            batch)
                shift 2
                _count=$#
                for _pair in "$@"; do
                    _key="${_pair%%=*}"
                    _value="${_pair#*=}"
                    config_set "$_key" "$_value" || exit $?
                done
                echo "keyforge: batch saved $_count keys"
                ;;
            get)
                [ -n "${3:-}" ] && config_get "$3" || cat "$CONF" 2>/dev/null
                ;;
            set)
                [ "$#" -ge 4 ] || {
                    echo "usage: keyforge.sh config set KEY VALUE" >&2
                    exit 2
                }
                config_set "$3" "$4" || exit $?
                echo "keyforge: $3 = $4"
                ;;
            *) cat "$CONF" 2>/dev/null ;;
        esac
        ;;

    devices)
        printf '{"devices":['
        _first=1
        _tmp="$(runtime_dir)/kf_devices.$$"
        if [ -x /system/bin/getevent ]; then
            /system/bin/getevent -i 2>/dev/null > "$_tmp"
        else
            getevent -i 2>/dev/null > "$_tmp"
        fi
        _name=""
        _vid=""
        _pid=""
        _handler=""
        while IFS= read -r line; do
            case "$line" in
                "add device"*)
                    emit_device
                    _handler="${line##* }"
                    _handler="${_handler##*/}"
                    _name=""
                    _vid=""
                    _pid=""
                    ;;
                *name:*)
                    _name="${line#*\"}"
                    _name="${_name%\"*}"
                    ;;
                *vendor*)
                    _vid="${line##* }"
                    case "$_vid" in 0x*) ;; *) _vid="0x${_vid}" ;; esac
                    ;;
                *product*)
                    _pid="${line##* }"
                    case "$_pid" in 0x*) ;; *) _pid="0x${_pid}" ;; esac
                    ;;
            esac
        done < "$_tmp"
        emit_device
        rm -f "$_tmp"
        printf ']}\n'
        ;;

    plugins)
        mkdir -p "$PLUGIN_DIR" 2>/dev/null
        case "${2:-}" in
            list)
                printf '{"plugins":['
                _first=1
                for _file in "$PLUGIN_DIR"/*.lua; do
                    [ -f "$_file" ] || continue
                    _name="$(basename "$_file" .lua)"
                    [ "$_first" = "1" ] && _first=0 || printf ','
                    printf '"%s"' "$(json_escape "$_name")"
                done
                printf ']}\n'
                ;;
            config)
                valid_key "${3:-}" || {
                    echo "keyforge: invalid plugin id" >&2
                    exit 2
                }
                cat "$DATA_DIR/configs/${3}.conf" 2>/dev/null
                ;;
            save-config)
                valid_key "${3:-}" || {
                    echo "keyforge: invalid plugin id" >&2
                    exit 2
                }
                _tmp="$DATA_DIR/configs/${3}.conf.tmp.$$"
                if printf '%s' "${4:-}" | base64 -d > "$_tmp" 2>/dev/null; then
                    chmod 600 "$_tmp"
                    mv -f "$_tmp" "$DATA_DIR/configs/${3}.conf"
                    echo "keyforge: saved ${3} settings"
                else
                    rm -f "$_tmp"
                    echo "keyforge: invalid settings payload" >&2
                    exit 2
                fi
                ;;
            upload)
                _name="$(basename "${3:-}")"
                case "$_name" in
                    *.lua) ;;
                    *) echo "keyforge: only .lua plugins are supported" >&2; exit 2 ;;
                esac
                valid_key "$_name" || {
                    echo "keyforge: plugin filename must use letters, numbers, dot, dash, or underscore" >&2
                    exit 2
                }
                _tmp="$PLUGIN_DIR/$_name.tmp.$$"
                if printf '%s' "${4:-}" | base64 -d > "$_tmp" 2>/dev/null; then
                    chmod 600 "$_tmp"
                    mv -f "$_tmp" "$PLUGIN_DIR/$_name"
                    echo "keyforge: installed $_name"
                else
                    rm -f "$_tmp"
                    echo "keyforge: invalid plugin payload" >&2
                    exit 2
                fi
                ;;
            install)
                [ -f "${3:-}" ] || {
                    echo "keyforge: plugin file not found" >&2
                    exit 2
                }
                _name="$(basename "$3")"
                case "$_name" in
                    *.lua) ;;
                    *) echo "keyforge: only .lua plugins are supported" >&2; exit 2 ;;
                esac
                cp "$3" "$PLUGIN_DIR/$_name" 2>/dev/null &&
                    echo "keyforge: installed $_name" ||
                    { echo "keyforge: install failed" >&2; exit 1; }
                ;;
            remove)
                valid_key "${3:-}" || {
                    echo "keyforge: invalid plugin id" >&2
                    exit 2
                }
                rm -f "$PLUGIN_DIR/${3}.lua" 2>/dev/null
                echo "keyforge: removed ${3}"
                ;;
            enable|disable)
                valid_key "${3:-}" || {
                    echo "keyforge: invalid plugin id" >&2
                    exit 2
                }
                _value=0
                [ "$2" = "enable" ] && _value=1
                config_set "plugin.${3}" "$_value" || exit $?
                "$0" restart
                ;;
            *) echo "usage: keyforge.sh plugins {list|config|save-config|upload|install|remove|enable|disable} ..." ;;
        esac
        ;;

    calibrate)
        case "${2:-}" in
            left)
                _raw="$(runtime_dir)/keyforge_raw_L"
                [ -f "$_raw" ] && read -r cx cy < "$_raw" 2>/dev/null
                [ -n "${cx:-}" ] && config_set calib_lx "$cx" &&
                    config_set calib_ly "$cy" &&
                    echo "keyforge: calibrate left x=$cx y=$cy"
                ;;
            right)
                _raw="$(runtime_dir)/keyforge_raw_R"
                [ -f "$_raw" ] && read -r cx cy < "$_raw" 2>/dev/null
                [ -n "${cx:-}" ] && config_set calib_rx "$cx" &&
                    config_set calib_ry "$cy" &&
                    echo "keyforge: calibrate right x=$cx y=$cy"
                ;;
            reset)
                config_set calib_lx 0
                config_set calib_ly 0
                config_set calib_rx 0
                config_set calib_ry 0
                echo "keyforge: calibration reset"
                ;;
            *) echo "usage: keyforge.sh calibrate {left|right|reset}" ;;
        esac
        ;;

    log)
        tail -80 "$LOG" 2>/dev/null
        ;;

    *)
        echo "usage: keyforge.sh {start|stop|restart|status|runtime|hide|manifest|config|devices|plugins|calibrate|log}"
        ;;
esac
