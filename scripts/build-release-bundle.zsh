#!/usr/bin/env zsh
# Build the versioned static Linux release archive and its SHA-256 receipt.
set -eu
setopt pipefail
umask 077

ROOT=${0:A:h:h}
TARGET=x86_64-unknown-linux-musl
TOOLCHAIN=1.89.0

if (( $# > 1 )); then
    print -u2 "usage: scripts/build-release-bundle.zsh [OUTPUT_DIRECTORY]"
    exit 2
fi

OUTPUT_DIR=${1:-$ROOT/dist}
for helper in cargo chmod cp cut grep gzip mkdir mktemp mv readelf sed sha256sum tar; do
    command -v $helper >/dev/null 2>&1 || {
        print -u2 -r -- "build-release-bundle: required helper not found: $helper"
        exit 1
    }
done

mkdir -p -- $OUTPUT_DIR
[[ ! -L $OUTPUT_DIR && -d $OUTPUT_DIR ]] || {
    print -u2 -r -- "build-release-bundle: output must be a regular directory: $OUTPUT_DIR"
    exit 1
}
OUTPUT_DIR=${OUTPUT_DIR:A}

VERSION=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' $ROOT/Cargo.toml)
if [[ -z $VERSION || $VERSION == *[!A-Za-z0-9._+-]* ]]; then
    print -u2 -r -- "build-release-bundle: could not read a safe package version from Cargo.toml"
    exit 1
fi

SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
if [[ -z $SOURCE_DATE_EPOCH || $SOURCE_DATE_EPOCH == *[!0-9]* ]]; then
    print -u2 "build-release-bundle: SOURCE_DATE_EPOCH must be a non-negative integer"
    exit 1
fi

print -r -- "build-release-bundle: building oopsinput $VERSION for $TARGET with Rust $TOOLCHAIN"
env RUSTUP_TOOLCHAIN=$TOOLCHAIN cargo build \
    --manifest-path $ROOT/Cargo.toml \
    --release \
    --locked \
    --target $TARGET

RELEASE_BIN=$ROOT/target/$TARGET/release/oopsinput
[[ -x $RELEASE_BIN && -f $RELEASE_BIN && ! -L $RELEASE_BIN ]] || {
    print -u2 -r -- "build-release-bundle: release binary missing at $RELEASE_BIN"
    exit 1
}
if readelf -l -- $RELEASE_BIN 2>/dev/null | grep -Fq INTERP; then
    print -u2 "build-release-bundle: release binary has a dynamic interpreter"
    exit 1
fi
if readelf -d -- $RELEASE_BIN 2>/dev/null | grep -Fq NEEDED; then
    print -u2 "build-release-bundle: release binary has a dynamic-library dependency"
    exit 1
fi

TMP_BASE=${TMPDIR:-/tmp}
[[ -d $TMP_BASE ]] || {
    print -u2 -r -- "build-release-bundle: temporary directory root does not exist: $TMP_BASE"
    exit 1
}
TMP_BASE=${TMP_BASE:A}
STAGE_ROOT=$(mktemp -d "$TMP_BASE/oopsinput-release.XXXXXX")
ARCHIVE_TMP=""
SUMS_TMP=""

cleanup() {
    [[ -z ${ARCHIVE_TMP:-} ]] || rm -f -- $ARCHIVE_TMP
    [[ -z ${SUMS_TMP:-} ]] || rm -f -- $SUMS_TMP
    if [[ -n ${STAGE_ROOT:-} && -d $STAGE_ROOT && ${STAGE_ROOT:h} == $TMP_BASE && \
          ${STAGE_ROOT:t} == oopsinput-release.* ]]; then
        rm -rf -- $STAGE_ROOT
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

TOP=oopsinput-$VERSION
STAGE=$STAGE_ROOT/$TOP
mkdir -- $STAGE
cp -- $RELEASE_BIN $STAGE/oopsinput
cp -- $ROOT/zsh/oopsinput.zsh $STAGE/oopsinput.zsh
cp -- $ROOT/zsh/install.zsh $STAGE/install.zsh
cp -- $ROOT/zsh/uninstall.zsh $STAGE/uninstall.zsh
cp -- $ROOT/LICENSE $STAGE/LICENSE
sed "s/@VERSION@/$VERSION/g" $ROOT/release/INSTALL.md > $STAGE/INSTALL.md

chmod 755 -- $STAGE $STAGE/oopsinput $STAGE/install.zsh $STAGE/uninstall.zsh
chmod 644 -- $STAGE/oopsinput.zsh $STAGE/LICENSE $STAGE/INSTALL.md

ARCHIVE_NAME=$TOP-$TARGET.tar.gz
ARCHIVE=$OUTPUT_DIR/$ARCHIVE_NAME
SUMS=$OUTPUT_DIR/SHA256SUMS
ARCHIVE_TMP=$(mktemp "$OUTPUT_DIR/.$ARCHIVE_NAME.XXXXXX")
SUMS_TMP=$(mktemp "$OUTPUT_DIR/.SHA256SUMS.XXXXXX")

(
    cd -- $STAGE_ROOT
    tar \
        --sort=name \
        --format=gnu \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --mtime="@$SOURCE_DATE_EPOCH" \
        -cf - \
        $TOP
) | gzip -n > $ARCHIVE_TMP

DIGEST=$(sha256sum $ARCHIVE_TMP | cut -d' ' -f1)
print -r -- "$DIGEST  $ARCHIVE_NAME" > $SUMS_TMP
chmod 644 -- $ARCHIVE_TMP $SUMS_TMP
mv -f -- $ARCHIVE_TMP $ARCHIVE
ARCHIVE_TMP=""
mv -f -- $SUMS_TMP $SUMS
SUMS_TMP=""

trap - EXIT HUP INT TERM
cleanup
print -r -- "build-release-bundle: wrote $ARCHIVE"
print -r -- "build-release-bundle: wrote $SUMS"
