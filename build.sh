#!/bin/bash
# Builds kernel8.img and installs it on the SD card if it is mounted.
#
#   ./build.sh           build, then install if the card is mounted
#   ./build.sh --eject   also eject the card after installing
#   ./build.sh --net     build, then install over the network (tools/deploy.py, to
#                        $RUSTYPI_HOST) instead of on the card
set -euo pipefail
cd "$(dirname "$0")"

TARGET=aarch64-unknown-none-softfloat
IMAGE=target/kernel8.img
BOOTFS=/Volumes/bootfs

EJECT=0
NET=0
for arg in "$@"; do
    case "$arg" in
        --eject) EJECT=1 ;;
        --net) NET=1 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

cargo build --release
rust-objcopy --strip-all -O binary "target/$TARGET/release/RustyPI" "$IMAGE"
echo "Built $IMAGE ($(wc -c < "$IMAGE" | tr -d ' ') bytes)"

if [ "$NET" = 1 ]; then
    exec tools/deploy.py "$IMAGE"
fi

if [ ! -d "$BOOTFS" ]; then
    echo -e "\033[33mSD card not mounted at $BOOTFS, skipped install\033[0m" >&2
    exit 0
fi

# -X skips extended attributes, which macOS would otherwise store on FAT as a ._kernel8.img
# file. dot_clean -m removes any ._* files already there (from Finder copies, say).
cp -X "$IMAGE" "$BOOTFS/kernel8.img"
dot_clean -m "$BOOTFS"
sync
cmp -s "$IMAGE" "$BOOTFS/kernel8.img" || { echo "Install failed: $BOOTFS/kernel8.img differs" >&2; exit 1; }
echo "Installed to $BOOTFS/kernel8.img"

if [ "$EJECT" = 1 ]; then
    diskutil eject "$BOOTFS"
fi
