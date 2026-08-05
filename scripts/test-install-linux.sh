#!/bin/sh
set -eu

release_directory=$1
test_directory=$(mktemp -d "${TMPDIR:-/tmp}/codexshim-installer-test.XXXXXX")
trap 'rm -rf "$test_directory"' EXIT HUP INT TERM

sh "$(dirname "$0")/install.sh" --release-dir "$release_directory" --install-dir "$test_directory"
sh "$(dirname "$0")/install.sh" --release-dir "$release_directory" --install-dir "$test_directory"
test -x "$test_directory/codexshim"
"$test_directory/codexshim" --version >/dev/null
