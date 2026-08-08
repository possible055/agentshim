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
            echo "installer did not report the expected Unix path on attempt $attempt" >&2
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

expect_unsupported_platform() {
    fake_uname_directory="$test_directory/fake-uname-$1-$2"
    mkdir -p "$fake_uname_directory"
    cat > "$fake_uname_directory/uname" <<EOF
#!/bin/sh
case "\$1" in
    -s) printf '%s\n' "$1" ;;
    -m) printf '%s\n' "$2" ;;
    *) exit 1 ;;
esac
EOF
    chmod 755 "$fake_uname_directory/uname"
    if PATH="$fake_uname_directory:$PATH" sh "$(dirname "$0")/install.sh" \
        --release-dir "$release_directory" --install-dir "$test_directory" >/dev/null 2>&1; then
        echo "installer unexpectedly accepted unsupported platform $1/$2" >&2
        exit 1
    fi
}

expect_unsupported_platform Plan9 x86_64
expect_unsupported_platform Linux mips64
