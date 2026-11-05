#!/usr/bin/env bash
# Fails when a raw sleep is ADDED to test code outside the sanctioned helpers.
#
# The test-stability rule: waits must be event-driven (poll with a deadline), not
# fixed sleeps. Poll helpers live in crates/test-support/src and
# adapters/dsh/tests/helpers; every other new sleep in test code needs a
# `sleep-allow:` comment (same line or the line above) stating why no observable
# event exists — a deliberate race window, a negative-assertion window, scenario
# pacing. The gate only inspects lines added between the two SHAs, so untouched
# legacy sites never fail it.
#
# Test code means dedicated test trees (tests/, benches/, and the sibling
# tests.rs / tests/ conventions inside product crates) and, in product .rs files,
# everything from the first inline `mod tests` block onward. Only the inline class
# also flags tokio::time::sleep: production code legitimately sleeps on tokio, and
# only the test region of the file is a violation there.
#
# Usage: check-test-sleeps.sh <base-sha> <head-sha>
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <base-sha> <head-sha>" >&2
    exit 2
fi
base=$1
head=$2

changed=$(git diff --name-only "$base" "$head")
# Test-only trees, plus the sibling `tests.rs` / `tests/` conventions inside
# product crates.
test_files=$(printf '%s\n' "$changed" |
    grep -E '(^tests/|^benches/|^crates/[^/]+/tests/|^adapters/dsh/tests/)|(/tests/|/tests\.rs$)' |
    grep -E '\.(rs|ts)$' || true)

# Product .rs files whose added lines may sit inside an inline `mod tests`.
inline_files=$(printf '%s\n' "$changed" |
    grep -E '^(src/|crates/[^/]+/src/)' |
    grep -E '\.rs$' |
    grep -vE '(^tests/|^benches/|/tests/|/tests\.rs$|^crates/test-support/src/)' || true)

status=0
while IFS= read -r file; do
    [ -n "$file" ] || continue
    case "$file" in
        # The sanctioned homes of raw sleeps: the poll helpers themselves.
        crates/test-support/src/* | adapters/dsh/tests/helpers/*) continue ;;
    esac
    git diff "$base" "$head" -- "$file" | awk -v file="$file" '
        /^\+\+\+|^---/ { next }
        /^ /           { prev = substr($0, 2); next }
        /^-/           { next }
        /^\+/ {
            line = substr($0, 2)
            is_rs_sleep = line ~ /thread::sleep[ \t]*\(/
            is_ts_sleep = line ~ /setTimeout\(/ && line ~ /[0-9]/
            if ((is_rs_sleep || is_ts_sleep) &&
                line !~ /sleep-allow:/ && prev !~ /sleep-allow:/) {
                printf "%s: new sleep without a sleep-allow marker: %s\n", file, line
                bad = 1
            }
            prev = line
            next
        }
        END { exit bad }
    ' || status=1
done <<EOF
$test_files
EOF

while IFS= read -r file; do
    [ -n "$file" ] || continue
    # First inline test module in the head version of the file; empty when the
    # file declares its tests in a sibling file or was deleted in this diff.
    boundary=$(git show "$head:$file" 2>/dev/null | awk '
        prev ~ /^#\[cfg\(test\)\]$/ && /^mod tests([ ]*[{]|$)/ { print NR; exit }
        /^mod tests[ ]*[{]$/ { print NR; exit }
        { prev = $0 }
    ' || true)
    [ -n "$boundary" ] || continue
    git diff "$base" "$head" -- "$file" | awk -v file="$file" -v boundary="$boundary" '
        /^\+\+\+|^---/ { next }
        /^@@ / {
            split(substr($0, index($0, "+") + 1), range, ",")
            newline = range[1] + 0
            next
        }
        /^ / { newline++; prev = substr($0, 2); next }
        /^-/ { next }
        /^\+/ {
            line = substr($0, 2)
            is_sleep = line ~ /thread::sleep[ \t]*\(/ || line ~ /tokio::time::sleep[ \t]*\(/
            if (newline >= boundary && is_sleep &&
                line !~ /sleep-allow:/ && prev !~ /sleep-allow:/) {
                printf "%s: new sleep in an inline test module without a sleep-allow marker: %s\n", file, line
                bad = 1
            }
            newline++
            prev = line
            next
        }
        END { exit bad }
    ' || status=1
done <<EOF
$inline_files
EOF

if [ "$status" -ne 0 ]; then
    echo "test-sleep gate: fixed sleeps are not allowed in test code; poll with a deadline" >&2
    echo "(crates/test-support helpers, or add a 'sleep-allow: <reason>' comment when no event exists)" >&2
fi
exit "$status"
