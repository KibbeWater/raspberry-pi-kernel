#!/usr/bin/env python3
"""Runs commands on RustyPI over its UDP console (port 2323) and prints what comes back.

The console answers each line and also sends whoever used it last a copy of everything the
Pi prints, so program output and kernel messages arrive too. Output ends once the Pi has been
quiet for --quiet seconds (or at --timeout). A command's own reply comes when it finishes, so
the first answer is waited for longer (--first).

    tools/pi.py version                 one command
    tools/pi.py --quiet 6 countdown 5   wait out a slow program
    printf 'sh\\nls\\nexit\\n' | tools/pi.py -    lines from stdin, one at a time

The Pi's address comes from --host, else $RUSTYPI_HOST. Whoever used the console last gets
its copy of the output: this script takes that over from an open `nc` session.
"""

import argparse
import os
import socket
import sys
import time

PORT = 2323


def run(sock, address, line, quiet, first, timeout):
    """Sends `line` and prints what comes back. Returns whether anything did."""
    sock.sendto((line + "\n").encode(), address)
    start = last = time.monotonic()
    answered = False
    while True:
        now = time.monotonic()
        if now - start > timeout:
            break
        if answered and now - last > quiet:
            break
        if not answered and now - start > max(quiet, first):
            break
        try:
            data, _ = sock.recvfrom(65535)
        except socket.timeout:
            continue
        sys.stdout.write(data.decode("utf-8", errors="replace"))
        sys.stdout.flush()
        answered = True
        last = time.monotonic()
    return answered


def main():
    parser = argparse.ArgumentParser(description="Run commands on RustyPI over its UDP console.")
    parser.add_argument("command", nargs="+", help="the command line, or - to read lines from stdin")
    parser.add_argument("--host", default=os.environ.get("RUSTYPI_HOST"), help="the Pi's address")
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--quiet", type=float, default=1.0, help="seconds of silence that end the output")
    parser.add_argument("--first", type=float, default=10.0,
                        help="seconds to wait for a first answer (a command's reply comes when it is done)")
    parser.add_argument("--timeout", type=float, default=60.0, help="most seconds to wait per line")
    args = parser.parse_args()
    if not args.host:
        parser.error("no address: pass --host or set RUSTYPI_HOST")

    lines = [line.rstrip("\n") for line in sys.stdin] if args.command == ["-"] else [" ".join(args.command)]
    address = (args.host, args.port)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(0.1)
    silent = False
    for line in lines:
        if not run(sock, address, line, args.quiet, args.first, args.timeout):
            silent = True
    if silent:
        print(f"(no answer from {args.host}:{args.port} to at least one line)", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
