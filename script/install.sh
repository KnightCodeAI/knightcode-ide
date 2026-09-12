#!/usr/bin/env sh
set -eu

# Unpacks a KnightCode tarball built by script/bundle-linux into ~/.local/.
# Point ZED_BUNDLE_PATH at the tarball; script/install-linux builds one and
# does that for you. There is no KnightCode download server yet.

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"
    channel="${ZED_CHANNEL:-stable}"
    # Use TMPDIR if available (for environments with non-standard temp directories)
    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        temp="$(mktemp -d "$TMPDIR/knightcode-XXXXXX")"
    else
        temp="$(mktemp -d "/tmp/knightcode-XXXXXX")"
    fi

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    case "$platform-$arch" in
        macos-arm64* | linux-arm64* | linux-aarch64)
            arch="aarch64"
            ;;
        macos-x86* | linux-x86*)
            arch="x86_64"
            ;;
        *)
            echo "Unsupported platform or architecture"
            exit 1
            ;;
    esac

    "$platform" "$@"

    if [ "$(command -v knightcode-ide)" = "$HOME/.local/bin/knightcode-ide" ]; then
        echo "KnightCode has been installed. Run with 'knightcode-ide'"
    else
        echo "To run KnightCode from your terminal, you must add ~/.local/bin to your PATH"
        echo "Run:"

        case "$SHELL" in
            *zsh)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "   source ~/.zshrc"
                ;;
            *fish)
                echo "   fish_add_path -U $HOME/.local/bin"
                ;;
            *)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "   source ~/.bashrc"
                ;;
        esac

        echo "To run KnightCode now, '~/.local/bin/knightcode-ide'"
    fi
}

linux() {
    if [ -z "${ZED_BUNDLE_PATH:-}" ]; then
        echo "There is no KnightCode download server yet. Build a tarball with script/bundle-linux and set ZED_BUNDLE_PATH to it."
        exit 1
    fi
    cp "$ZED_BUNDLE_PATH" "$temp/knightcode-linux-$arch.tar.gz"

    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    # Matches release_channel::app_id() and the .desktop file bundle-linux writes.
    appid=""
    case "$channel" in
      stable)
        appid="dev.knightcode.KnightCode"
        ;;
      nightly)
        appid="dev.knightcode.KnightCode-Nightly"
        ;;
      preview)
        appid="dev.knightcode.KnightCode-Preview"
        ;;
      dev)
        appid="dev.knightcode.KnightCode-Dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="dev.knightcode.KnightCode"
        ;;
    esac

    # Unpack
    rm -rf "$HOME/.local/knightcode$suffix.app"
    mkdir -p "$HOME/.local/knightcode$suffix.app"
    tar -xzf "$temp/knightcode-linux-$arch.tar.gz" -C "$HOME/.local/"

    zed_editor="$HOME/.local/knightcode$suffix.app/libexec/zed-editor"
    if [ -f "$zed_editor" ] && command -v ldd >/dev/null 2>&1; then
        missing="$(ldd "$zed_editor" 2>/dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
        if [ -n "$missing" ]; then
            echo "Warning: your system is missing libraries that KnightCode needs:"
            echo "$missing" | sed 's/^/    /'
            echo "Install them with your package manager, or KnightCode will fail to start."
        fi
    fi

    # Setup ~/.local directories
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

    # Link the binary
    ln -sf "$HOME/.local/knightcode$suffix.app/bin/knightcode-ide" "$HOME/.local/bin/knightcode-ide"

    # Copy .desktop file
    desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
    src_dir="$HOME/.local/knightcode$suffix.app/share/applications"
    cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
    sed -i "s|Icon=knightcode|Icon=$HOME/.local/knightcode$suffix.app/share/icons/hicolor/512x512/apps/knightcode.png|g" "${desktop_file_path}"
    sed -i "s|Exec=knightcode-ide|Exec=$HOME/.local/knightcode$suffix.app/bin/knightcode-ide|g" "${desktop_file_path}"
}

macos() {
    echo "There is no KnightCode download server yet, and no macOS installer to fetch. Build a bundle with script/bundle-mac."
    exit 1
}

main "$@"
