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

if [[ "$platform" == macos-x64 || "$platform" == macos-arm64 ]]; then
    exec bash "$client_root/scripts/package-macos-app.sh" --platform "$platform"
fi

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
esac

echo "Packaged ${platform} artifact at ${archive_path}"
