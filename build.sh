#!/bin/bash
cargo build --release

echo "\nStripping EFI binary"
rust-objcopy --strip-all -O binary target/armv7a-none-eabi/release/RustyPI target/kernel.img