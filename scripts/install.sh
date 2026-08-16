#!/bin/sh
set -eu

version=""
release_directory=""
install_directory="${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin"

# Injected by the release workflow from the release tag; empty in source.
default_version="" # @agentshim:default-version

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            version=$2
            shift 2
            ;;
        --install-dir)
            install_directory=$2
            shift 2
            ;;
        --release-dir)
            release_directory=$2
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

operating_system=$(uname -s)
architecture=$(uname -m)
case "$operating_system:$architecture" in
    Linux:x86_64)
        target="x86_64-unknown-linux-gnu"
        ;;
    Linux:aarch64)
        target="aarch64-unknown-linux-gnu"
        ;;
    Darwin:arm64)
        target="aarch64-apple-darwin"
        ;;
    *)
        echo "unsupported platform for agentshim installer: $operating_system/$architecture" >&2
        exit 1
        ;;
esac

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/agentshim-install.XXXXXX")
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

if [ -n "$release_directory" ]; then
    set -- "$release_directory"/agentshim-*-$target.tar.gz
    if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
        echo "expected exactly one $target release archive in $release_directory" >&2
        exit 1
    fi
    archive_path=$1
    checksum_path="$archive_path.sha256"
    if [ ! -f "$checksum_path" ]; then
        echo "missing checksum file: $checksum_path" >&2
        exit 1
    fi
else
    command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
    resolved_version=$version
    if [ -z "$resolved_version" ]; then
        resolved_version=$default_version
    fi
    if [ -n "$resolved_version" ]; then
        case "$resolved_version" in
            v*) tag=$resolved_version ;;
            *) tag="v$resolved_version" ;;
        esac
    else
        release_json=$(curl --proto '=https' --tlsv1.2 -fsSL \
            -H 'Accept: application/vnd.github+json' \
            https://api.github.com/repos/possible055/agentshim/releases/latest)
        tag=$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
        if [ -z "$tag" ]; then
            echo "could not determine the latest agentshim release" >&2
            exit 1
        fi
    fi
    release_version=${tag#v}
    archive_name="agentshim-$release_version-$target.tar.gz"
    base_url="https://github.com/possible055/agentshim/releases/download/$tag"
    archive_path="$temporary_directory/$archive_name"
    checksum_path="$archive_path.sha256"
    curl --proto '=https' --tlsv1.2 -fsSL "$base_url/$archive_name" -o "$archive_path"
    curl --proto '=https' --tlsv1.2 -fsSL "$base_url/$archive_name.sha256" -o "$checksum_path"
fi

archive_name=$(basename "$archive_path")
expected_hash=$(awk -v name="$archive_name" '
    {
        file = $2
        sub(/^\*/, "", file)
        if (file == name && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/) {
            print tolower($1)
            exit
        }
    }
' "$checksum_path")
if [ -z "$expected_hash" ]; then
    echo "invalid checksum file: $checksum_path" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "$archive_path" | awk '{print tolower($1)}')
elif command -v shasum >/dev/null 2>&1; then
    actual_hash=$(shasum -a 256 "$archive_path" | awk '{print tolower($1)}')
else
    echo "a SHA-256 utility (sha256sum or shasum) is required" >&2
    exit 1
fi
if [ "$actual_hash" != "$expected_hash" ]; then
    echo "checksum verification failed for $archive_name" >&2
    exit 1
fi

extract_directory="$temporary_directory/extract"
mkdir -p "$extract_directory"
tar -xzf "$archive_path" -C "$extract_directory"
binary_path=$(find "$extract_directory" -type f -name agentshim -print)
if [ "$(printf '%s\n' "$binary_path" | sed '/^$/d' | wc -l)" -ne 1 ]; then
    echo "the release archive does not contain exactly one agentshim executable" >&2
    exit 1
fi

mkdir -p "$install_directory"
staged="$install_directory/.agentshim.$$"
cp "$binary_path" "$staged"
chmod 755 "$staged"
"$staged" --version >/dev/null
destination="$install_directory/agentshim"
mv -f "$staged" "$destination"

echo "Installed agentshim at $destination"
echo "Set command = \"$destination\" in your Codex MCP configuration."
