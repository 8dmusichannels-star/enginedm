#!/bin/sh
set -eu
(set -o pipefail) 2>/dev/null && set -o pipefail
cd "$(dirname "$0")"

usage() {
    cat <<'EOF'
Usage:
  ./install.sh --install              ask, item by item, what to install
  ./install.sh --remove               ask, item by item, what to remove
  ./install.sh --install --yes-all    install everything, no questions
  ./install.sh --remove --yes-all     remove everything, no questions
  ./install.sh --help

--yes-all answers 'yes' to every question, including the ones that default to 'no'
(for --remove that deletes enginedm.conf and /var/lib/enginedm too). The flags may be
given in either order.
EOF
}

MODE=
YES_ALL=0
for arg in "$@"; do
    case "$arg" in
        --help|-h)
            usage
            exit 0
            ;;
        --install|--remove)
            if [ -n "$MODE" ] && [ "$MODE" != "$arg" ]; then
                echo "Use either --install or --remove, not both." >&2
                usage >&2
                exit 2
            fi
            MODE=$arg
            ;;
        --yes-all)
            YES_ALL=1
            ;;
        *)
            echo "Unknown option: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$MODE" ]; then
    usage
    exit 2
fi

if [ "$YES_ALL" -eq 0 ] && [ ! -t 0 ]; then
    echo "$MODE asks questions and needs an interactive terminal (or use --yes-all)." >&2
    exit 1
fi

# ---------------------------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------------------------

# ask "question" default(y|n): returns 0 for yes. Enter takes the default.
ask() {
    _q=$1
    _d=$2
    if [ "$YES_ALL" -eq 1 ]; then
        echo "$_q yes (--yes-all)"
        return 0
    fi
    if [ "$_d" = y ]; then _hint="[Y/n]"; else _hint="[y/N]"; fi
    while :; do
        printf '%s %s ' "$_q" "$_hint"
        IFS= read -r _a || _a=
        case "$_a" in
            "")
                [ "$_d" = y ] && return 0
                return 1
                ;;
            y|Y|yes|YES) return 0 ;;
            n|N|no|NO) return 1 ;;
            *) echo "Please answer y or n." ;;
        esac
    done
}

# section "Title" "detail line" ...: header printed before each question.
section() {
    echo
    echo "== $1"
    shift
    for _l in "$@"; do
        echo "   $_l"
    done
}

# any_exists path...: true when at least one of the paths exists.
any_exists() {
    for _p in "$@"; do
        if [ -e "$_p" ] || [ -L "$_p" ]; then
            return 0
        fi
    done
    return 1
}

install_cfg() {
    sudo rm -f "$3"
    sudo install -Dm"$1" "$2" "$3"
}

HAVE_OPENRC=0
if command -v openrc-run >/dev/null 2>&1; then
    HAVE_OPENRC=1
fi
HAVE_SYSTEMD=0
if command -v systemctl >/dev/null 2>&1; then
    HAVE_SYSTEMD=1
fi

GREETER_MARKER=/usr/share/enginedm/.greeter-created

# =============================================================================================
# --remove
# =============================================================================================
if [ "$MODE" = "--remove" ]; then
    if [ "$YES_ALL" -eq 1 ]; then
        echo "EngineDM removal: --yes-all, removing EVERYTHING, including enginedm.conf and /var/lib/enginedm."
    else
        echo "EngineDM removal. Every item is asked separately; nothing is deleted without a 'y'."
    fi
    echo "Items that are not installed are skipped."
    REMOVED=0

    # Stop the services before their files go away.
    if any_exists /etc/init.d/enginedm; then
        section "OpenRC service" \
            "Stops and disables the service (rc-service stop, rc-update del) and removes:" \
            "  /etc/init.d/enginedm" \
            "Stopping it closes the greeter; a running user session is not affected."
        if ask "Remove the OpenRC service?" y; then
            if command -v rc-service >/dev/null 2>&1; then
                sudo rc-service enginedm stop 2>/dev/null || true
                sudo rc-update del enginedm default 2>/dev/null || true
            fi
            sudo rm -f /etc/init.d/enginedm
            REMOVED=1
        fi
    fi

    if any_exists /etc/systemd/system/enginedm.service; then
        section "systemd service" \
            "Stops and disables the service (systemctl stop, disable) and removes:" \
            "  /etc/systemd/system/enginedm.service" \
            "Disabling also drops the display-manager.service alias, so enable your other" \
            "display manager afterwards if you want one."
        if ask "Remove the systemd service?" y; then
            if command -v systemctl >/dev/null 2>&1; then
                sudo systemctl stop enginedm.service 2>/dev/null || true
                sudo systemctl disable enginedm.service 2>/dev/null || true
            fi
            sudo rm -f /etc/systemd/system/enginedm.service
            if command -v systemctl >/dev/null 2>&1; then
                sudo systemctl daemon-reload 2>/dev/null || true
            fi
            REMOVED=1
        fi
    fi

    if any_exists /usr/libexec/enginedm-run /usr/libexec/enginedm-weston /etc/conf.d/enginedm; then
        section "Run loop, Weston wrapper and settings" \
            "/usr/libexec/enginedm-run  /usr/libexec/enginedm-weston  /etc/conf.d/enginedm" \
            "The run loop is shared by the OpenRC and systemd services; the Weston wrapper is also" \
            "used by greetd mode."
        if ask "Remove them?" y; then
            sudo rm -f /usr/libexec/enginedm-run /usr/libexec/enginedm-weston /etc/conf.d/enginedm
            REMOVED=1
        fi
    fi

    if any_exists /usr/bin/enginedmgreeterd /usr/bin/configmanager; then
        section "Programs" "/usr/bin/enginedmgreeterd  /usr/bin/configmanager"
        if ask "Remove the programs?" y; then
            sudo rm -f /usr/bin/enginedmgreeterd /usr/bin/configmanager
            REMOVED=1
        fi
    fi

    if any_exists /usr/share/enginedm/interfaces; then
        section "Interfaces" \
            "/usr/share/enginedm/interfaces (the bundled Hyproman interface)." \
            "Interfaces you put in /etc/enginedm/interfaces or /usr/local/share/enginedm/interfaces" \
            "are never touched."
        if ask "Remove the bundled interfaces?" y; then
            sudo rm -rf /usr/share/enginedm/interfaces
            REMOVED=1
        fi
    fi

    if any_exists /etc/pam.d/enginedm; then
        section "PAM service" "/etc/pam.d/enginedm (used when pam_service=enginedm)"
        if ask "Remove the PAM service file?" y; then
            sudo rm -f /etc/pam.d/enginedm
            REMOVED=1
        fi
    fi

    if any_exists /etc/enginedm/weston.ini /etc/enginedm/sway.conf; then
        section "Greeter Weston config" \
            "/etc/enginedm/weston.ini (and the old /etc/enginedm/sway.conf from earlier versions)"
        if ask "Remove it?" y; then
            sudo rm -f /etc/enginedm/weston.ini /etc/enginedm/sway.conf
            REMOVED=1
        fi
    fi

    if any_exists /etc/greetd/enginedm-config.toml; then
        section "greetd example config" \
            "/etc/greetd/enginedm-config.toml (your own /etc/greetd/config.toml is not touched)"
        if ask "Remove it?" y; then
            sudo rm -f /etc/greetd/enginedm-config.toml
            REMOVED=1
        fi
    fi

    if any_exists /var/log/enginedm.log /var/log/enginedm-session.log /run/enginedm; then
        section "Logs and runtime files" \
            "/var/log/enginedm.log  /var/log/enginedm-session.log  /run/enginedm"
        if ask "Remove them?" y; then
            sudo rm -f /var/log/enginedm.log /var/log/enginedm-session.log
            sudo rm -rf /run/enginedm
            REMOVED=1
        fi
    fi

    if any_exists /etc/enginedm/enginedm.conf; then
        section "Configuration (your settings)" \
            "/etc/enginedm/enginedm.conf. It may contain your 2FA secret. Cannot be undone." \
            "Back it up first if you want to keep it."
        if ask "Delete the configuration?" n; then
            sudo rm -f /etc/enginedm/enginedm.conf
            REMOVED=1
        fi
    fi

    if any_exists /var/lib/enginedm; then
        section "State data" \
            "/var/lib/enginedm: unidata.scrub (theme list) and the HOTP counter." \
            "Deleting the HOTP counter lets old 2FA codes work again if you reinstall." \
            "Cannot be undone."
        if ask "Delete the state data?" n; then
            sudo rm -rf /var/lib/enginedm
            REMOVED=1
        fi
    fi

    if [ -e "$GREETER_MARKER" ]; then
        section "'greeter' user" \
            "install.sh created this system user, so it can remove it again."
        if ask "Delete the 'greeter' user and group?" y; then
            if command -v userdel >/dev/null 2>&1; then
                sudo userdel greeter 2>/dev/null || true
                if getent group greeter >/dev/null 2>&1; then
                    sudo groupdel greeter 2>/dev/null || true
                fi
            elif command -v deluser >/dev/null 2>&1; then
                sudo deluser greeter 2>/dev/null || true
                if getent group greeter >/dev/null 2>&1; then
                    sudo delgroup greeter 2>/dev/null || true
                fi
            fi
            sudo rm -f "$GREETER_MARKER"
            REMOVED=1
        fi
    fi

    # Directories left empty. rmdir only succeeds on empty ones, so your own files stay.
    for d in /etc/enginedm/interfaces /etc/enginedm /usr/local/share/enginedm/interfaces \
             /usr/local/share/enginedm /usr/share/enginedm; do
        sudo rmdir "$d" 2>/dev/null || true
    done

    echo
    if [ "$REMOVED" -eq 1 ]; then
        echo "Done. Whatever you answered 'n' to was left in place."
    else
        echo "Nothing was removed."
    fi
    exit 0
fi

# =============================================================================================
# --install
# =============================================================================================
if [ "$YES_ALL" -eq 1 ]; then
    echo "EngineDM installation: --yes-all, installing everything. Existing files are replaced."
else
    echo "EngineDM installation. Every item is asked separately."
    echo "Existing files are only replaced if you say so."
fi

# ---- programs --------------------------------------------------------------------------------
section "Programs" \
    "Builds with 'cargo build --release' and installs:" \
    "  /usr/bin/enginedmgreeterd   the greeter (also the session host after login)" \
    "  /usr/bin/configmanager      config tool"
if any_exists /usr/bin/enginedmgreeterd /usr/bin/configmanager; then
    echo "   (already installed: it would be overwritten)"
fi
if ask "Build and install the programs?" y; then
    cargo build --release
    install_cfg 755 target/release/enginedmgreeterd /usr/bin/enginedmgreeterd
    install_cfg 755 target/release/configmanager /usr/bin/configmanager
fi

# ---- interfaces ------------------------------------------------------------------------------
section "Interfaces" \
    "Copies the bundled Hyproman interface to /usr/share/enginedm/interfaces and creates the empty" \
    "folders /etc/enginedm/interfaces and /usr/local/share/enginedm/interfaces for your own."
if any_exists /usr/share/enginedm/interfaces; then
    echo "   (already installed: the bundled folder is replaced)"
fi
if ask "Install the interfaces?" y; then
    IFACE_DIR=/usr/share/enginedm/interfaces
    sudo rm -rf "$IFACE_DIR"
    sudo install -d "$IFACE_DIR"
    sudo install -d /etc/enginedm/interfaces /usr/local/share/enginedm/interfaces
    sudo cp -r interfaces/. "$IFACE_DIR"/
fi

# ---- PAM -------------------------------------------------------------------------------------
PAM_SRC=data/pam.d/enginedm
PAM_NOTE="pam_systemd"
# A system booted by systemd keeps pam_systemd; otherwise OpenRC systems use elogind.
if [ ! -d /run/systemd/system ] && [ "$HAVE_OPENRC" -eq 1 ]; then
    PAM_SRC=data/pam.d/enginedm.elogind
    PAM_NOTE="pam_elogind (OpenRC detected)"
fi
section "PAM service" \
    "/etc/pam.d/enginedm with $PAM_NOTE." \
    "Needed for standalone mode with pam_service=enginedm; greetd uses its own."
if any_exists /etc/pam.d/enginedm; then
    echo "   (already exists: it would be overwritten)"
fi
if ask "Install the PAM service?" y; then
    install_cfg 644 "$PAM_SRC" /etc/pam.d/enginedm
fi

# ---- run loop --------------------------------------------------------------------------------
section "Run loop, Weston wrapper and settings" \
    "/usr/libexec/enginedm-weston  starts Weston (kiosk-shell) + the greeter, stops Weston after it" \
    "                              (used by greetd mode and the run loop; needs 'weston' installed)" \
    "/usr/libexec/enginedm-run     standalone mode: greeter, then the user's session, in a loop" \
    "/etc/conf.d/enginedm          their settings (VT, weston arguments, seat backend)" \
    "The run loop is shared by the OpenRC and systemd services below."
if any_exists /usr/libexec/enginedm-run /usr/libexec/enginedm-weston /etc/conf.d/enginedm; then
    echo "   (already exists: these would be overwritten, including your /etc/conf.d/enginedm edits)"
fi
if ask "Install them?" y; then
    install_cfg 755 data/openrc/enginedm-weston /usr/libexec/enginedm-weston
    install_cfg 755 data/openrc/enginedm-run /usr/libexec/enginedm-run
    install_cfg 644 data/openrc/enginedm.confd /etc/conf.d/enginedm
fi

# ---- OpenRC ----------------------------------------------------------------------------------
if [ "$HAVE_OPENRC" -eq 1 ]; then
    section "OpenRC service (standalone mode)" \
        "/etc/init.d/enginedm" \
        "It is not enabled automatically; enable it with: sudo rc-update add enginedm default"
    if any_exists /etc/init.d/enginedm; then
        echo "   (already exists: it would be overwritten)"
    fi
    if ask "Install the OpenRC service?" y; then
        install_cfg 755 data/openrc/enginedm /etc/init.d/enginedm
        echo "   OpenRC service installed. Enable it with: sudo rc-update add enginedm default"
    fi
fi

# ---- systemd ---------------------------------------------------------------------------------
if [ "$HAVE_SYSTEMD" -eq 1 ]; then
    section "systemd service (standalone mode)" \
        "/etc/systemd/system/enginedm.service (then 'systemctl daemon-reload')" \
        "It is not enabled automatically. Enabling it replaces any other display manager:" \
        "  sudo systemctl enable enginedm.service && sudo systemctl set-default graphical.target"
    if any_exists /etc/systemd/system/enginedm.service; then
        echo "   (already exists: it would be overwritten)"
    fi
    if ask "Install the systemd service?" y; then
        install_cfg 644 data/systemd/enginedm.service /etc/systemd/system/enginedm.service
        sudo systemctl daemon-reload 2>/dev/null || true
        echo "   systemd service installed. Enable it with: sudo systemctl enable enginedm.service"
    fi
fi

# ---- weston ----------------------------------------------------------------------------------
section "Greeter Weston config" "/etc/enginedm/weston.ini (kiosk-shell config the greeter runs in)"
if any_exists /etc/enginedm/weston.ini; then
    echo "   (already exists: it would be overwritten)"
fi
if any_exists /etc/enginedm/sway.conf; then
    echo "   (/etc/enginedm/sway.conf from an earlier version is no longer used; --remove deletes it)"
fi
if ask "Install it?" y; then
    install_cfg 644 data/weston/weston.ini /etc/enginedm/weston.ini
fi
if ! command -v weston >/dev/null 2>&1; then
    echo "   NOTE: 'weston' is not installed. Install it (with kiosk-shell, Weston 10+) before starting."
fi

# ---- greeter user ----------------------------------------------------------------------------
# The `greeter` account differs between distributions (or does not exist), so offer to create
# it. System account, no login shell, home = EngineDM's state directory.
create_greeter_user() {
    NOLOGIN=$(command -v nologin 2>/dev/null || true)
    [ -n "$NOLOGIN" ] || NOLOGIN=/bin/false
    if command -v useradd >/dev/null 2>&1; then
        sudo useradd --system --user-group --no-create-home \
            --home-dir /var/lib/enginedm --shell "$NOLOGIN" greeter
    elif command -v adduser >/dev/null 2>&1 && case "$(adduser --help 2>&1 || true)" in *--system*) true ;; *) false ;; esac; then
        sudo adduser --system --group --no-create-home --home /var/lib/enginedm \
            --shell "$NOLOGIN" greeter
    elif command -v adduser >/dev/null 2>&1; then
        sudo addgroup -S greeter 2>/dev/null || true
        sudo adduser -S -D -H -h /var/lib/enginedm -s "$NOLOGIN" -G greeter greeter
    else
        echo "   Could not create the 'greeter' user (no useradd/adduser); create it by hand." >&2
        return 1
    fi
    for g in video input seat _seatd; do
        if getent group "$g" >/dev/null 2>&1; then
            sudo usermod -aG "$g" greeter 2>/dev/null || sudo adduser greeter "$g" 2>/dev/null || true
        fi
    done
    sudo install -d /usr/share/enginedm
    sudo touch "$GREETER_MARKER"
    echo "   Created system user 'greeter'."
}

if id greeter >/dev/null 2>&1; then
    echo
    echo "== 'greeter' user: already exists, nothing to do."
else
    section "'greeter' user" \
        "A system user without login shell, home /var/lib/enginedm, added to the video/input/seat" \
        "groups that exist. Standalone mode runs as root and does not need it; greetd mode does." \
        "--remove only deletes it again because install.sh created it."
    if ask "Create the 'greeter' user?" y; then
        create_greeter_user || true
    fi
fi

# ---- state dir, config, theme list -----------------------------------------------------------
section "State directory" \
    "/var/lib/enginedm and /var/lib/enginedm/unidata (mode 700; HOTP counter and theme list)." \
    "Owned by 'greeter' if that user exists."
if ask "Create it?" y; then
    sudo install -d -m700 /var/lib/enginedm /var/lib/enginedm/unidata
    if id greeter >/dev/null 2>&1; then
        sudo chown -R greeter:greeter /var/lib/enginedm
    fi
fi

section "Example configuration" \
    "/etc/enginedm/enginedm.conf from data/enginedm/enginedm.conf (every key explained)." \
    "Replaces your current file if there is one, so back it up first if you changed it."
if any_exists /etc/enginedm/enginedm.conf; then
    echo "   (already exists: it would be overwritten)"
fi
if ask "Install the example config?" y; then
    install_cfg 640 data/enginedm/enginedm.conf /etc/enginedm/enginedm.conf
    if id greeter >/dev/null 2>&1 && [ -e /etc/enginedm/enginedm.conf ]; then
        sudo chgrp greeter /etc/enginedm/enginedm.conf
        sudo chmod 640 /etc/enginedm/enginedm.conf
    fi
fi

section "Example theme list" \
    "/var/lib/enginedm/unidata/unidata.scrub (GTK theme folders to scan)." \
    "Replaces your current file if there is one."
if any_exists /var/lib/enginedm/unidata/unidata.scrub; then
    echo "   (already exists: it would be overwritten)"
fi
if ask "Install the example theme list?" y; then
    sudo install -d -m700 /var/lib/enginedm/unidata
    install_cfg 644 data/unidata/unidata.scrub /var/lib/enginedm/unidata/unidata.scrub
    if id greeter >/dev/null 2>&1; then
        sudo chown -R greeter:greeter /var/lib/enginedm
    fi
fi

# ---- greetd ----------------------------------------------------------------------------------
GREETD_DEFAULT=n
if command -v greetd >/dev/null 2>&1; then
    GREETD_DEFAULT=y
fi
section "greetd example config" \
    "/etc/greetd/enginedm-config.toml. Only for greetd mode; your /etc/greetd/config.toml" \
    "is not touched."
if any_exists /etc/greetd/enginedm-config.toml; then
    echo "   (already exists: it would be overwritten)"
fi
if ask "Install it?" "$GREETD_DEFAULT"; then
    install_cfg 644 data/greetd/config.toml /etc/greetd/enginedm-config.toml
fi

echo
echo "Done. Try: enginedmgreeterd --check"
