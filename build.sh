#!/bin/bash
set -e

TARGET=aarch64-unknown-none-softfloat
cargo build --release

echo "Stripping to raw binary"
rust-objcopy --strip-all -O binary target/$TARGET/release/RustyPI target/kernel8.img

if [ -d /Volumes/bootfs ]; then
    echo "Copying kernel8.img to /Volumes/bootfs"
    cp target/kernel8.img /Volumes/bootfs/kernel8.img
fi
