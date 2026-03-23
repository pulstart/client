#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: package-unix.sh --platform <linux-x64|macos-x64|macos-arm64>

Packages an existing release build from target/release/st-client into client/dist/.
EOF
}

platform=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform)
            platform="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

case "$platform" in
    linux-x64|macos-x64|macos-arm64) ;;
    *)
        echo "Missing or invalid --platform value: '$platform'" >&2
        usage >&2
        exit 1
        ;;
esac

client_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary_path="$client_root/target/release/st-client"
version="$(
    sed -n 's/^version = "\(.*\)"/\1/p' "$client_root/Cargo.toml" \
        | head -n 1
)"

if [[ -z "$version" ]]; then
    echo "Unable to resolve client version from Cargo.toml" >&2
    exit 1
fi

if [[ ! -f "$binary_path" ]]; then
    echo "Release binary not found at $binary_path" >&2
    echo "Build it first with: cargo build --release --locked" >&2
    exit 1
fi

dist_root="$client_root/dist"
staging_root="$dist_root/staging"
package_name="st-client-v${version}-${platform}"
package_root="$staging_root/$package_name"

mkdir -p "$staging_root"
rm -rf "$package_root"
mkdir -p "$package_root"

case "$platform" in
    linux-x64)
        cp "$binary_path" "$package_root/st-client"
        chmod 755 "$package_root/st-client"
        cat > "$package_root/README.txt" <<'EOF'
This archive contains the Linux x64 build of st-client.

The Linux package is built on GitHub Actions Ubuntu runners and ships as a plain tarball.
Runtime libraries are not bundled. Install FFmpeg, Opus, and the normal desktop OpenGL/audio
stack on the target machine before launching the client.
EOF
        archive_path="$dist_root/${package_name}.tar.gz"
        rm -f "$archive_path"
        tar -C "$staging_root" -czf "$archive_path" "$package_name"
        ;;
    macos-x64|macos-arm64)
        app_root="$package_root/st-client.app"
        mkdir -p "$app_root/Contents/MacOS"
        cp "$binary_path" "$app_root/Contents/MacOS/st-client"
        chmod 755 "$app_root/Contents/MacOS/st-client"
        cat > "$app_root/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>st-client</string>
    <key>CFBundleIdentifier</key>
    <string>com.pulstart.st-client</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>st-client</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
        perl -0pi -e "s#<string>0\\.1\\.0</string>#<string>${version}</string>#g" \
            "$app_root/Contents/Info.plist"
        cat > "$package_root/README.txt" <<'EOF'
This archive contains the macOS build of st-client packaged as a .app bundle.

The app bundle is created on GitHub Actions macOS runners. Runtime Homebrew dependencies are
not bundled yet. Install them on the target machine before launching the client:

    brew install ffmpeg opus
EOF
        archive_path="$dist_root/${package_name}.zip"
        rm -f "$archive_path"
        ditto -c -k --sequesterRsrc --keepParent "$package_root" "$archive_path"
        ;;
esac

echo "Packaged ${platform} artifact at ${archive_path}"
