#!/usr/bin/env bash

detect_init_system() {
    if pidof systemd 1>/dev/null 2>/dev/null; then
        INIT_SYSTEM="systemd"
    elif [ -f "/sbin/rc-update" ]; then
        INIT_SYSTEM="openrc"
    else
        INIT_SYSTEM="other"
    fi
}

install() {
    echo "Building the project..."
    cargo build --release # TODO: The GUI should be optional. At least for now. Before releasing this, it sould be turned into a feature with an explicit cli switch to install it

    if [ $? -ne 0 ]; then
        echo "An error occurred while building the project"
        exit 1
    fi

    # Stop a running razer-settings (window or tray process) so the upgrade
    # does not leave an old GUI talking to the new daemon for the session.
    if pgrep -x razer-settings > /dev/null 2>&1; then
        echo "Stopping running razer-settings for the upgrade..."
        pkill -x razer-settings || true
    fi

    # Stop the service if it's running
    echo "Stopping the service..."
    case $INIT_SYSTEM in
    systemd)
        systemctl --user stop razercontrol
        ;;
    openrc)
        sudo rc-service razercontrol stop
        ;;
    esac

    # Install the files
    echo "Installing the files..."
    mkdir -p ~/.local/share/razercontrol
    sudo bash <<'INSTALL_FILES'
        # Abort on the first failure: without this, a failed copy mid-block is
        # swallowed (the block's exit status is only the LAST command's) and
        # the script would report success over a half-upgraded install.
        set -e
        mkdir -p /usr/share/razercontrol
        # install(1), not cp: it unlinks the destination first, so a running
        # old binary keeps its inode. Overwriting an in-use file with cp is
        # the classic source of mysterious crashes, and the kernel's ETXTBSY
        # guard is explicitly a courtesy feature, not a contract.
        install -m755 target/release/razer-cli /usr/bin/razer-cli
        install -m755 target/release/razer-settings /usr/bin/razer-settings
        install -m755 target/release/daemon /usr/bin/razer-daemon
        if ls /usr/share/applications/*.desktop 1> /dev/null 2>&1; then
            # We only install the desktop file if there are already desktop
            # files on the system
            cp data/gui/com.encomjp.razer-settings.desktop /usr/share/applications/
        fi
        install -Dm644 data/gui/com.github.encomjp.razercontrol.svg /usr/share/icons/hicolor/scalable/apps/com.github.encomjp.razercontrol.svg
        cp data/devices/laptops.json /usr/share/razercontrol/
        # udev rule renamed 99→70: uaccess tags are processed by
        # 73-seat-late.rules, so the file must sort before 73 to work at all.
        # Drop the legacy file from earlier installs so no stale rule lingers.
        rm -f /etc/udev/rules.d/99-hidraw-permissions.rules
        cp data/udev/70-razercontrol.rules /etc/udev/rules.d/
        udevadm control --reload-rules
        # Apply the reloaded rules to already-present hidraw nodes: without
        # this a FIRST install has wrong node permissions until reboot or
        # replug, and the user daemon loops on "no supported device found".
        udevadm trigger --subsystem-match=hidraw
INSTALL_FILES

    if [ $? -ne 0 ]; then
        echo "An error occurred while installing the files"
        exit 1
    fi

    # Start the service
    echo "Starting the service..."
    case $INIT_SYSTEM in
    systemd)
        sudo cp data/services/systemd/razercontrol.service /usr/lib/systemd/user/
        # Without a reload the user manager may not know the freshly copied
        # unit yet and `enable` fails on a fresh install.
        systemctl --user daemon-reload
        systemctl --user enable --now razercontrol
        ;;
    openrc)
        sudo bash <<EOF
            cp data/services/openrc/razercontrol /etc/init.d/
            # HACK: Change the username in the script
            sed -i 's/USERNAME_CHANGEME/$USER/' /etc/init.d/razercontrol

            chmod +x /etc/init.d/razercontrol
            rc-update add razercontrol default
            rc-service razercontrol start
EOF
        ;;
    esac

    # The power-mode key feature reads /dev/input/event* from the user daemon;
    # evdev nodes are root:input and are deliberately NOT covered by uaccess.
    if ! id -nG | grep -qw input; then
        echo ""
        echo "NOTE: your user is not in the 'input' group, so the power-mode"
        echo "      key will be inactive (everything else works). To enable it:"
        echo "          sudo usermod -aG input \$USER"
        echo "      then log out and back in."
    fi

    echo "Installation complete"
}

uninstall() {
    # Stop a running razer-settings (window or tray) first — mirrors the
    # install path; otherwise the GUI keeps running from a deleted binary.
    if pgrep -x razer-settings > /dev/null 2>&1; then
        echo "Stopping running razer-settings..."
        pkill -x razer-settings || true
    fi

    # Stop the service first so nothing keeps running from deleted binaries
    echo "Stopping the service..."
    case $INIT_SYSTEM in
    systemd)
        systemctl --user disable --now razercontrol
        sudo rm -f /usr/lib/systemd/user/razercontrol.service
        systemctl --user daemon-reload
        ;;
    openrc)
        sudo bash <<UNINST_RC
            rc-service razercontrol stop
            rc-update del razercontrol default
            rm -f /etc/init.d/razercontrol
UNINST_RC
        ;;
    esac

    # Remove the files (icon and data directory included)
    echo "Uninstalling the files..."
    sudo bash <<'UNINST_FILES'
        set -e
        rm -f /usr/bin/razer-cli
        rm -f /usr/bin/razer-settings
        rm -f /usr/share/applications/com.encomjp.razer-settings.desktop
        rm -f /usr/share/icons/hicolor/scalable/apps/com.github.encomjp.razercontrol.svg
        rm -f /usr/bin/razer-daemon
        rm -f /usr/share/razercontrol/laptops.json
        rmdir --ignore-fail-on-non-empty /usr/share/razercontrol
        rm -f /etc/udev/rules.d/70-razercontrol.rules
        # Legacy name from installs before the 99→70 rename:
        rm -f /etc/udev/rules.d/99-hidraw-permissions.rules
        udevadm control --reload-rules
        # Reset existing hidraw nodes right away — without the trigger they
        # keep the removed rule's group/ACL until replug or reboot.
        udevadm trigger --subsystem-match=hidraw
UNINST_FILES

    if [ $? -ne 0 ]; then
        echo "An error occurred while uninstalling the files"
        exit 1
    fi

    echo "Uninstalled. Per-user configuration was kept at ~/.local/share/razercontrol"
    echo "(remove it manually if you want a full wipe)."
    echo "Note: your user stays in the 'input' group (power-mode key requirement)."
    echo "      Remove with: sudo gpasswd -d \$USER input"
}

main() {
    if [ "$EUID" -eq 0 ]; then
        echo "Please do not run as root"
        exit 1
    fi

    detect_init_system

    if [ "$INIT_SYSTEM" = "other" ]; then
        echo "Unsupported init system"
        exit 1
    fi

    case $1 in
    install)
        install
        ;;
    uninstall)
        uninstall
        ;;
    *)
        echo "Usage: $0 {install|uninstall}"
        exit 1
        ;;
    esac
}

main "$@"
