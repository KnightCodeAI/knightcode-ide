#!/usr/bin/env sh
set -eu

# Uninstalls KnightCode that was installed using the install.sh script

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        # Check for any KnightCode variants in /Applications
        remaining=$(ls -d /Applications/KnightCode*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        # Check for any KnightCode variants in ~/.local
        remaining=$(ls -d "$HOME/.local/knightcode"*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

prompt_remove_preferences() {
    printf "Do you want to keep your KnightCode preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/knightcode"
            echo "Preferences removed."
            ;;
        *)
            echo "Preferences kept."
            ;;
    esac
}

main() {
    platform="$(uname -s)"
    channel="${ZED_CHANNEL:-stable}"

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    "$platform"

    echo "KnightCode has been uninstalled"
}

# Paths follow paths::APP_NAME ("KnightCode", lowercased for XDG directories)
# and release_channel::app_id(). ~/.zed_server is not removed: KnightCode ships
# no remote server, and that directory belongs to an installed Zed.

linux() {
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    db_suffix="stable"
    case "$channel" in
      stable)
        appid="dev.knightcode.KnightCode"
        db_suffix="stable"
        ;;
      nightly)
        appid="dev.knightcode.KnightCode-Nightly"
        db_suffix="nightly"
        ;;
      preview)
        appid="dev.knightcode.KnightCode-Preview"
        db_suffix="preview"
        ;;
      dev)
        appid="dev.knightcode.KnightCode-Dev"
        db_suffix="dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="dev.knightcode.KnightCode"
        db_suffix="stable"
        ;;
    esac

    # Remove the app directory
    rm -rf "$HOME/.local/knightcode$suffix.app"

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/knightcode-ide"

    # Remove the .desktop file
    rm -f "$HOME/.local/share/applications/${appid}.desktop"

    # Remove the database directory for this channel
    rm -rf "$HOME/.local/share/knightcode/db/0-$db_suffix"

    # Remove socket file
    rm -f "$HOME/.local/share/knightcode/zed-$db_suffix.sock"

    # Remove the entire KnightCode directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/.local/share/knightcode"
        prompt_remove_preferences
    fi
}

macos() {
    app="KnightCode.app"
    db_suffix="stable"
    app_id="dev.knightcode.KnightCode"
    case "$channel" in
      nightly)
        app="KnightCode Nightly.app"
        db_suffix="nightly"
        app_id="dev.knightcode.KnightCode-Nightly"
        ;;
      preview)
        app="KnightCode Preview.app"
        db_suffix="preview"
        app_id="dev.knightcode.KnightCode-Preview"
        ;;
      dev)
        app="KnightCode Dev.app"
        db_suffix="dev"
        app_id="dev.knightcode.KnightCode-Dev"
        ;;
    esac

    # Remove the app bundle
    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/knightcode-ide"

    # Remove the database directory for this channel
    rm -rf "$HOME/Library/Application Support/KnightCode/db/0-$db_suffix"

    # Remove app-specific files and directories
    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    # Remove the entire KnightCode directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/Library/Application Support/KnightCode"
        rm -rf "$HOME/Library/Logs/KnightCode"

        prompt_remove_preferences
    fi
}

main "$@"
