#!/bin/bash
cargo rustc --target aarch64-unknown-none-softfloat --release

echo "\nStripping EFI binary"
rust-objcopy --strip-all -O binary target/armv7a-none-eabi/release/RustyPI target/kernel.img