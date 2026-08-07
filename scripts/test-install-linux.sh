#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 RELEASE_DIRECTORY EXPECTED_VERSION" >&2
    exit 2
fi

release_directory=$1
expected_version=$2
test_directory=$(mktemp -d "${TMPDIR:-/tmp}/codexshim-installer-test.XXXXXX")
trap 'rm -rf "$test_directory"' EXIT HUP INT TERM

expected_binary_version="codexshim $expected_version"
for attempt in 1 2; do
    installer_output=$(sh "$(dirname "$0")/install.sh" --release-dir "$release_directory" --install-dir "$test_directory")
    expected_path="$test_directory/codexshim"
    case "$installer_output" in
        *"Installed codexshim at $expected_path"*) ;;
        *)
            echo "installer did not report the expected Linux path on attempt $attempt" >&2
            exit 1
            ;;
    esac
    test -x "$expected_path"
    version_output=$("$expected_path" --version)
    if [ "$version_output" != "$expected_binary_version" ]; then
        echo "installed executable reported '$version_output'; expected '$expected_binary_version'" >&2
        exit 1
    fi
done
