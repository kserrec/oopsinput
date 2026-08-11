#!/usr/bin/env zsh
# Verify the release receipt, static binary, archive boundary, and the complete
# install-to-uninstall journey using only files extracted from the archive.
set -eu
setopt pipefail
umask 077
export LC_ALL=C

ROOT=${0:A:h:h}
TARGET=x86_64-unknown-linux-musl

if (( $# != 1 )); then
    print -u2 "usage: scripts/release-bundle-gate.zsh ARCHIVE"
    exit 2
fi

fail() {
    print -u2 -r -- "release-bundle-gate: FAIL: $1"
    exit 1
}

for helper in cmp grep mktemp readelf sed sha256sum sort stat tar wc zsh; do
    command -v $helper >/dev/null 2>&1 || fail "required helper not found: $helper"
done

ARCHIVE=${1:A}
[[ -f $ARCHIVE && ! -L $ARCHIVE ]] || fail "archive is not a regular file: $ARCHIVE"
ARCHIVE_NAME=${ARCHIVE:t}
[[ $ARCHIVE_NAME == oopsinput-*-$TARGET.tar.gz ]] || fail "unexpected archive name: $ARCHIVE_NAME"
VERSION=${ARCHIVE_NAME#oopsinput-}
VERSION=${VERSION%-$TARGET.tar.gz}
[[ -n $VERSION && $VERSION != *[!A-Za-z0-9._+-]* ]] || fail "archive version is invalid"
TOP=oopsinput-$VERSION

SUMS=${ARCHIVE:h}/SHA256SUMS
[[ -f $SUMS && ! -L $SUMS ]] || fail "SHA256SUMS is not a regular file beside the archive"
[[ $(wc -l < $SUMS) -eq 1 ]] || fail "SHA256SUMS must contain exactly one entry"
SUM_LINE=$(< $SUMS)
EXPECTED_DIGEST=${SUM_LINE%%  *}
SUMMED_NAME=${SUM_LINE#*  }
[[ "$EXPECTED_DIGEST  $SUMMED_NAME" == $SUM_LINE ]] || fail "SHA256SUMS has an invalid entry"
[[ ${#EXPECTED_DIGEST} -eq 64 && $EXPECTED_DIGEST != *[!0-9a-f]* ]] || fail "SHA-256 digest is invalid"
[[ $SUMMED_NAME == $ARCHIVE_NAME ]] || fail "SHA256SUMS names a different archive"
ACTUAL_LINE=$(sha256sum $ARCHIVE)
ACTUAL_DIGEST=${ACTUAL_LINE%% *}
[[ $ACTUAL_DIGEST == $EXPECTED_DIGEST ]] || fail "archive digest does not match SHA256SUMS"

EXPECTED_LIST=$(print -l -- \
    "$TOP/" \
    "$TOP/INSTALL.md" \
    "$TOP/LICENSE" \
    "$TOP/install.zsh" \
    "$TOP/oopsinput" \
    "$TOP/oopsinput.zsh" \
    "$TOP/uninstall.zsh" | sort)
ACTUAL_LIST=$(tar -tzf $ARCHIVE | sort)
[[ $ACTUAL_LIST == $EXPECTED_LIST ]] || fail "archive contents differ from the release contract"

TMP_BASE=${TMPDIR:-/tmp}
[[ -d $TMP_BASE ]] || fail "temporary directory root does not exist: $TMP_BASE"
TMP_BASE=${TMP_BASE:A}
GATE_ROOT=$(mktemp -d "$TMP_BASE/oopsinput-release-gate.XXXXXX")
cleanup() {
    if [[ -n ${GATE_ROOT:-} && -d $GATE_ROOT && ${GATE_ROOT:h} == $TMP_BASE && \
          ${GATE_ROOT:t} == oopsinput-release-gate.* ]]; then
        rm -rf -- $GATE_ROOT
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# The gate's private umask must not rewrite the modes stored in the archive;
# those stored modes are part of the release contract being tested.
tar --same-permissions -xzf $ARCHIVE -C $GATE_ROOT
BUNDLE=$GATE_ROOT/$TOP
[[ -d $BUNDLE && ! -L $BUNDLE ]] || fail "archive top level is not a regular directory"
for file in INSTALL.md LICENSE install.zsh oopsinput oopsinput.zsh uninstall.zsh; do
    [[ -f $BUNDLE/$file && ! -L $BUNDLE/$file ]] || fail "archive member is not a regular file: $file"
done

[[ $(stat -c %a -- $BUNDLE/oopsinput) == 755 ]] || fail "binary mode is not 755"
[[ $(stat -c %a -- $BUNDLE/install.zsh) == 755 ]] || fail "installer mode is not 755"
[[ $(stat -c %a -- $BUNDLE/uninstall.zsh) == 755 ]] || fail "uninstaller mode is not 755"
[[ $(stat -c %a -- $BUNDLE/oopsinput.zsh) == 644 ]] || fail "plugin archive mode is not 644"
[[ $(stat -c %a -- $BUNDLE/LICENSE) == 644 ]] || fail "license mode is not 644"
[[ $(stat -c %a -- $BUNDLE/INSTALL.md) == 644 ]] || fail "install readme mode is not 644"

cmp -s -- $ROOT/zsh/install.zsh $BUNDLE/install.zsh || fail "archive installer differs from the repository source"
cmp -s -- $ROOT/zsh/uninstall.zsh $BUNDLE/uninstall.zsh || fail "archive uninstaller differs from the repository source"
cmp -s -- $ROOT/zsh/oopsinput.zsh $BUNDLE/oopsinput.zsh || fail "archive plugin differs from the repository source"
cmp -s -- $ROOT/LICENSE $BUNDLE/LICENSE || fail "archive license differs from the repository source"
grep -Fq -- "# Install oopsinput $VERSION" $BUNDLE/INSTALL.md || fail "install readme has the wrong version"

VERSION_OUT=$($BUNDLE/oopsinput version)
[[ $VERSION_OUT == "oopsinput $VERSION" ]] || fail "binary version does not match the archive"
if readelf -l -- $BUNDLE/oopsinput 2>/dev/null | grep -Fq INTERP; then
    fail "release binary has a dynamic interpreter"
fi
if readelf -d -- $BUNDLE/oopsinput 2>/dev/null | grep -Fq NEEDED; then
    fail "release binary has a dynamic-library dependency"
fi

$ROOT/scripts/lifecycle-gate.zsh $BUNDLE

trap - EXIT HUP INT TERM
cleanup
print -r -- "GATE PASS: $ARCHIVE_NAME is static, complete, verified, and installable"
