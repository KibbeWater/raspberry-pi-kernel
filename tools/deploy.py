#!/usr/bin/env python3
"""Installs a kernel on RustyPI over the network and waits for it to come back up.

    tools/deploy.py                     target/kernel8.img to $RUSTYPI_HOST
    tools/deploy.py --host 192.168.0.64 path/to/kernel8.img

It arms the Pi with `update` on the console (port 2323), sends the image to port 2324 in
acknowledged chunks with its CRC-32, and once the Pi has installed it (keeping the old one as
/kernel8.bak) and rebooted, checks that `version` answers with the one in the image sent.
"""

import argparse
import os
import socket
import struct
import sys
import time
import zlib

sys.dont_write_bytecode = True  # no __pycache__ in the repo from importing pi
import pi  # noqa: E402

UPDATE_PORT = 2324
CHUNK = 1024
REPLY_TIMEOUT = 0.5
RETRIES = 10
INSTALL_TIMEOUT = 30.0
BOOT_TIMEOUT = 90.0


def exchange(sock, address, datagram, accept, timeout=REPLY_TIMEOUT, retries=RETRIES):
    """Sends `datagram` until an answer `accept` takes comes back. Returns that answer."""
    for _ in range(retries):
        sock.sendto(datagram, address)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            sock.settimeout(max(deadline - time.monotonic(), 0.01))
            try:
                answer, _ = sock.recvfrom(2048)
            except socket.timeout:
                break
            if answer.startswith(b"X"):
                sys.exit(f"deploy: the Pi refused: {answer[1:].decode(errors='replace')}")
            if accept(answer):
                return answer
    sys.exit("deploy: no answer from the Pi")


def console(host, line, first=5.0):
    """Sends `line` to the console and returns what came back."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(0.1)
    sock.sendto((line + "\n").encode(), (host, pi.PORT))
    text, start, last = "", time.monotonic(), None
    while time.monotonic() - start < first + 1.0:
        try:
            data, _ = sock.recvfrom(65535)
        except socket.timeout:
            if last is not None and time.monotonic() - last > 0.5:
                break
            continue
        text += data.decode(errors="replace")
        last = time.monotonic()
    return text


def main():
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser(description="Install a kernel on RustyPI over the network.")
    parser.add_argument("image", nargs="?", default=os.path.join(repo, "target", "kernel8.img"))
    parser.add_argument("--host", default=os.environ.get("RUSTYPI_HOST"), help="the Pi's address")
    args = parser.parse_args()
    if not args.host:
        parser.error("no address: pass --host or set RUSTYPI_HOST")

    image = open(args.image, "rb").read()
    crc = zlib.crc32(image)
    print(f"deploy: {args.image}, {len(image)} bytes, CRC-32 {crc:08x}")

    armed = console(args.host, "!update")
    if "update: ready" not in armed:
        sys.exit(f"deploy: the Pi didn't arm for an update: {armed.strip() or 'no answer'}")

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    address = (args.host, UPDATE_PORT)
    exchange(sock, address, b"B" + struct.pack("<II", len(image), crc), lambda a: a == b"K")
    start = time.monotonic()
    for offset in range(0, len(image), CHUNK):
        ack = b"A" + struct.pack("<I", offset)
        exchange(sock, address, b"D" + struct.pack("<I", offset) + image[offset:offset + CHUNK], lambda a: a == ack)
        done = offset + CHUNK
        if done % (64 * CHUNK) == 0:
            print(f"deploy: sent {done // 1024} KB")
    elapsed = time.monotonic() - start
    print(f"deploy: sent {len(image)} bytes in {elapsed:.1f} s; installing")
    exchange(sock, address, b"E", lambda a: a == b"F", timeout=INSTALL_TIMEOUT, retries=1)
    print("deploy: installed; the Pi is rebooting")

    time.sleep(3)
    deadline = time.monotonic() + BOOT_TIMEOUT
    while time.monotonic() < deadline:
        answer = console(args.host, "!version", first=1.0).strip()
        if answer:
            print(f"deploy: back up: {answer}")
            # The version it reports is built into the image: finding it there says the Pi runs
            # what was sent.
            version = answer.removeprefix("RustyPI ").encode()
            if version not in image:
                sys.exit("deploy: that isn't the version in the image sent")
            return
        time.sleep(1)
    sys.exit("deploy: the Pi didn't come back (check the screen or the serial link)")


if __name__ == "__main__":
    main()
