# USB on RustyPI

Where USB support stands, what the hardware is, and the plan from here to a keyboard and
Ethernet.

## The hardware

- **Controller:** a Synopsys DesignWare USB 2.0 OTG core (DWC2) at `0x3F98_0000`. Core
  registers are at +0x000, host at +0x400, and the power and clock gate at +0xE00. The vendor
  ID register reads `0x4F54_280A` (core 2.80a). It has 8 host channels and does DMA only
  (hardware architecture 2). The firmware must power it on first: mailbox tag `0x0002_8001`,
  device 3.
- **One root port.** Everything shares it, at up to 480 Mb/s.
- **The 3B+ bus:** the root port leads to the LAN7515. That chip is a 4-port hub with a
  3-port hub behind it, and behind that the LAN7800 gigabit Ethernet controller (`0424:7800`,
  limited to about 300 Mb/s by the bus). The four USB sockets hang off the two hubs, so a
  keyboard is always one or two hubs deep.
- **DMA:** the controller sees memory through the VideoCore's bus addresses. It gets the
  `0xC000_0000` alias (`BusAddress::from_arm`), which bypasses the GPU's L2 cache. It doesn't
  see the ARM caches either, so buffers are cleaned before the controller reads them and
  invalidated after it writes them.
- **Interrupt:** GPU IRQ 9, which reaches core 0 only (like the UART).

## What exists

| Piece | Where |
|---|---|
| Power on, the MAC address, DMA bus addresses | `rustypi-core/src/mailbox` |
| Setup packets, device/configuration/hub descriptors, port status, boot keyboard reports, US key map | `rustypi-core/src/usb.rs` (host tested) |
| DWC2 bring-up: power, core reset, forced host mode, FIFOs, root port reset; polled control transfers by DMA on channel 0 | `src/drivers/usb.rs` |
| Enumeration and hubs: addresses, configurations, port power and reset, walking the tree; generic over a `Bus`, tested against a pretend 3B+ bus | `rustypi-core/src/usb/tree.rs` |
| `usb` command: starts the controller, enumerates the bus, prints it as a tree | `src/sys/usb.rs`, `src/commands/usb.rs` |

Steps 1 to 4 below are done. USB starts at boot in the background (`sys::usb`); `usb` shows
the bus as found then. A boot protocol keyboard is polled every 10ms (interrupt transfers,
split through its hub's translator on the microframe schedule), and what is typed is echoed on
the screen and handed to the shell a line at a time, in the layout `keyboard` chose (US or
Swedish), with accents, key repeat, and Ctrl+C (stop the foreground program), Ctrl+U and
Ctrl+L. Hubs are polled every 500ms for devices plugged in or pulled out.

Steps 5 and 6 are written, not yet tried on the Pi: bulk transfers, the LAN7800 driver
(`src/drivers/lan7800.rs`), and a network stack in `rustypi-core/src/net` (Ethernet, ARP,
IPv4, ICMP, UDP, DHCP), run by a task in `src/sys/net.rs`. `net` shows the link and address,
`ping` pings.

## The plan

Each step ends with something to see on the Pi.

1. **Enumeration.** Give devices addresses (`SET_ADDRESS`), read their configurations, pick
   one (`SET_CONFIGURATION`). This needs a device tree in the kernel: address, speed, parent
   hub and port, endpoint 0 packet size, configuration. *Check:* `usb` lists the root hub.
2. **Hubs.** A hub driver: power the ports, wait for power good, and for each connected port
   reset it, learn its speed and enumerate what's there, recursively. Change detection by
   polling `GET_PORT_STATUS` at first (the hub's interrupt endpoint later). *Check:* `usb`
   shows both LAN7515 hubs, the LAN7800, and whatever is plugged in, as a tree.
3. **Split transactions.** A low or full speed device behind a high speed hub is reached
   through the hub's transaction translator. The channel's split register names the hub
   address and port, and each transaction becomes a start-split plus complete-splits, with
   NYET retries. A keyboard needs this. *Check:* a keyboard's descriptors read.
4. **Interrupt transfers and the keyboard.** Periodic polling of the keyboard's interrupt IN
   endpoint at its interval, with data toggle kept per endpoint. Set it to the boot protocol
   (`SET_PROTOCOL`, `SET_IDLE`), decode reports (`KeyboardReport`, `key_to_char`), and feed
   the characters into the shell as another input next to the Arduino link. A kernel task on
   a sleep timer does the polling first; the controller's interrupt, with SOF scheduling,
   comes later. *Check:* typing on a USB keyboard runs commands, and a foreground program
   reads it.
5. **Bulk transfers and the LAN7800.** Registers go through vendor control requests (0xA1
   read, 0xA0 write, 4 bytes, register in `wIndex`). Init: reset, MAC address from the
   firmware, FIFOs, receive filter, PHY reset and autonegotiation, then TX and RX on. Frames
   go out on the bulk OUT endpoint behind an 8-byte header and come in on bulk IN behind a
   10-byte one. *Check:* the link LED lights, and the Pi answers ARP.
6. **A small network stack.** Ethernet, ARP, IPv4, ICMP echo and UDP first (ping, and a UDP
   console), each layer a pure parser in `rustypi-core` with host tests. TCP after that.
   *Check:* `ping` from the Mac.

## Risks and notes

- **Cache maintenance** is the classic DMA bug. Every buffer the controller touches must be
  cache-line aligned and sized, and cleaned or invalidated at the right moment. `DmaBuffer`
  does this for the probe; transfers from the heap will need the same.
- **NAKs.** In DMA mode the controller retries NAKed non-periodic transactions itself, except
  for split transactions, which halt the channel. Polled code must retry those.
- **One channel** is enough for enumeration and a keyboard. Ethernet at speed needs several
  in flight, so a channel allocator comes with step 5.
- **Interrupts vs polling.** Everything is polled so far, which ties up a core per transfer.
  That's fine for enumeration and a keyboard; Ethernet will want the controller's IRQ and a
  wait queue per channel.
- **Multicore:** the host controller belongs to one task at a time (a sleeping `Mutex`), since
  a control transfer is a sequence of stages on a shared channel.

## References

- Circle's DWHCI driver and LAN7800 driver, the closest bare-metal reference:
  <https://github.com/rsta2/circle> (`lib/usb/dwhcidevice.cpp`, `include/circle/usb/dwhci.h`,
  `lib/usb/lan7800.cpp`).
- Linux `drivers/usb/dwc2` and `drivers/net/usb/lan78xx.c`.
- USB 2.0 specification, chapters 9 (devices) and 11 (hubs); HID 1.11, appendix B (boot
  protocol).
- USB on the Raspberry Pi:
  <https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#universal-serial-bus-usb>.
