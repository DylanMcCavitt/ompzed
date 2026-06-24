#!/usr/bin/env sh
set -eu

# Uninstalls Ompzed that was installed using the install.sh script

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        # Check for any Ompzed variants in /Applications
        remaining=$(ls -d /Applications/Ompzed*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        # Check for any Ompzed variants in ~/.local (bundle dir name is build
        # output, still `zed*.app`; see docs/src/omp/distribution-identity.md)
        remaining=$(ls -d "$HOME/.local/zed"*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

prompt_remove_preferences() {
    printf "Do you want to keep your Ompzed preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/zed"
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

    echo "Ompzed has been uninstalled"
}

linux() {
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    db_suffix="stable"
    case "$channel" in
      stable)
        appid="dev.ompzed.Ompzed"
        db_suffix="stable"
        ;;
      nightly)
        appid="dev.ompzed.Ompzed-Nightly"
        db_suffix="nightly"
        ;;
      preview)
        appid="dev.ompzed.Ompzed-Preview"
        db_suffix="preview"
        ;;
      dev)
        appid="dev.ompzed.Ompzed-Dev"
        db_suffix="dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="dev.ompzed.Ompzed"
        db_suffix="stable"
        ;;
    esac

    # Remove the app directory
    rm -rf "$HOME/.local/zed$suffix.app"

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/zed"

    # Remove the .desktop file
    rm -f "$HOME/.local/share/applications/${appid}.desktop"

    # Remove the database directory for this channel (data dir is APP_NAME-keyed)
    rm -rf "$HOME/.local/share/ompzed/db/0-$db_suffix"

    # Remove socket file (the `zed-` filename prefix is hardcoded in the app)
    rm -f "$HOME/.local/share/ompzed/zed-$db_suffix.sock"

    # Remove the entire Ompzed data directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/.local/share/ompzed"
        prompt_remove_preferences
    fi

    # `.zed_server` is the hardcoded remote-host server dir (not APP_NAME-keyed);
    # see docs/src/omp/distribution-identity.md.
    rm -rf $HOME/.zed_server
}

macos() {
    app="Ompzed.app"
    db_suffix="stable"
    app_id="dev.ompzed.Ompzed"
    case "$channel" in
      nightly)
        app="Ompzed Nightly.app"
        db_suffix="nightly"
        app_id="dev.ompzed.Ompzed-Nightly"
        ;;
      preview)
        app="Ompzed Preview.app"
        db_suffix="preview"
        app_id="dev.ompzed.Ompzed-Preview"
        ;;
      dev)
        app="Ompzed Dev.app"
        db_suffix="dev"
        app_id="dev.ompzed.Ompzed-Dev"
        ;;
    esac

    # Remove the app bundle
    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/zed"

    # Remove the database directory for this channel (data dir is APP_NAME-keyed)
    rm -rf "$HOME/Library/Application Support/Ompzed/db/0-$db_suffix"

    # Remove app-specific files and directories
    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    # Remove the entire Ompzed directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/Library/Application Support/Ompzed"
        rm -rf "$HOME/Library/Logs/Ompzed"

        prompt_remove_preferences
    fi

    rm -rf $HOME/.zed_server
}

main "$@"
