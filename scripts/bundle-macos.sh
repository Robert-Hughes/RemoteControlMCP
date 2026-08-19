#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/bundle-macos.sh [--dev|--release] [--install]

Build RemoteControlMCP as a macOS .app bundle. By default this only creates the
bundle under target/ and does not register or install it anywhere else.

Options:
  --dev      Build and bundle the development (debug) profile (default).
  --debug    Alias for --dev.
  --release  Build and bundle the release profile.
  --install  Also update ~/Applications/RemoteControlMCP.app and register the
             bundle with LaunchServices.
  -h, --help Show this help text.
EOF
}

profile="debug"
profile_explicit="false"
install_bundle="false"

while (( $# > 0 )); do
    case "$1" in
        --dev|--debug)
            if [[ "$profile_explicit" == "true" && "$profile" != "debug" ]]; then
                echo "error: --dev/--debug and --release are mutually exclusive" >&2
                exit 2
            fi
            profile="debug"
            profile_explicit="true"
            ;;
        --release)
            if [[ "$profile_explicit" == "true" && "$profile" != "release" ]]; then
                echo "error: --dev/--debug and --release are mutually exclusive" >&2
                exit 2
            fi
            profile="release"
            profile_explicit="true"
            ;;
        --install)
            install_bundle="true"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: macOS bundling must be run on macOS" >&2
    exit 1
fi

script_dir="$(cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

app_name="RemoteControlMCP"
bundle_id="com.xman2.remotecontrolmcp"
binary_name="remote-control-mcp"
icon_source="$repo_root/assets/app-icon.png"
binary="$repo_root/target/$profile/$binary_name"
bundle="$repo_root/target/$profile/bundle/macos/$app_name.app"
contents="$bundle/Contents"
applications_dir="$HOME/Applications"
applications_link="$applications_dir/$app_name.app"
lsregister="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

version="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
if [[ -z "$version" ]]; then
    echo "error: could not determine package version from Cargo.toml" >&2
    exit 1
fi

if [[ ! -f "$icon_source" ]]; then
    echo "error: missing application icon: $icon_source" >&2
    exit 1
fi

if [[ "$profile" == "release" ]]; then
    cargo build --release
else
    cargo build
fi

if [[ ! -x "$binary" ]]; then
    echo "error: cargo build did not produce executable: $binary" >&2
    exit 1
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/remotecontrolmcp-bundle.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT
iconset="$tmp_dir/$app_name.iconset"
mkdir -p "$iconset"

make_icon() {
    local filename="$1"
    local size="$2"
    /usr/bin/sips -s format png -z "$size" "$size" "$icon_source" --out "$iconset/$filename" >/dev/null
}

make_icon icon_16x16.png 16
make_icon icon_16x16@2x.png 32
make_icon icon_32x32.png 32
make_icon icon_32x32@2x.png 64
make_icon icon_128x128.png 128
make_icon icon_128x128@2x.png 256
make_icon icon_256x256.png 256
make_icon icon_256x256@2x.png 512
make_icon icon_512x512.png 512
make_icon icon_512x512@2x.png 1024

/usr/bin/iconutil -c icns "$iconset" -o "$tmp_dir/$app_name.icns"

rm -rf "$bundle"
mkdir -p "$contents/MacOS" "$contents/Resources"
/usr/bin/install -m 755 "$binary" "$contents/MacOS/$binary_name"
/usr/bin/install -m 644 "$tmp_dir/$app_name.icns" "$contents/Resources/$app_name.icns"

cat > "$contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$app_name</string>
    <key>CFBundleExecutable</key>
    <string>$binary_name</string>
    <key>CFBundleIconFile</key>
    <string>$app_name.icns</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$app_name</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

/usr/bin/codesign --force --sign - "$bundle"

printf 'Bundled %s profile:\n  %s\n' "$profile" "$bundle"

if [[ "$install_bundle" == "true" ]]; then
    mkdir -p "$applications_dir"
    if [[ -e "$applications_link" && ! -L "$applications_link" ]]; then
        echo "error: refusing to replace non-symlink Applications entry: $applications_link" >&2
        exit 1
    fi
    ln -sfn "$bundle" "$applications_link"
    "$lsregister" -f "$bundle"
    printf 'Installed Applications entry:\n  %s -> %s\nRegistered with LaunchServices.\n' \
        "$applications_link" "$bundle"
else
    printf 'Not installed or registered. Use --install to opt in.\n'
fi
