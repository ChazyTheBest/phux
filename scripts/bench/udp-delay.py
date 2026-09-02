#!/usr/bin/env python3
"""Userspace UDP relay that adds a fixed one-way delay, for WAN simulation.

Loopback QUIC has a sub-millisecond round trip, so a loopback benchmark
measures per-frame CPU overhead and hides everything that costs a round trip:
a serialized handshake, a per-key request/response, a first paint that will
not fit in one congestion window. Shaping the loopback interface would need
`dnctl`/`pfctl` and therefore root, which a benchmark must not require, so
this relay does the same job entirely in userspace and without privileges.

It binds one UDP socket, forwards every datagram to `--to`, and forwards the
replies back to whichever address last spoke to it, holding each datagram for
`--delay-ms` before it goes out. Delay is applied in *both* directions, so a
relay started with `--delay-ms 25` adds 50 ms to a round trip.

Datagrams are neither reordered nor dropped: each one is scheduled on the
event loop at `now + delay`, and the loop fires timers in time order, so a
QUIC sender sees a clean high-latency path rather than a lossy one. That is
the property under test — phux's round-trip count and payload size — not the
loss recovery quinn already owns.

Stdlib only, so it runs from the same scrubbed `env -i` the harness uses.

  udp-delay.py --listen 127.0.0.1:19001 --to 127.0.0.1:19000 --delay-ms 25
"""

import argparse
import asyncio
import socket
import sys


def parse_addr(text):
    """Split HOST:PORT into the (host, port) tuple asyncio wants."""
    host, _, port = text.rpartition(":")
    if not host or not port.isdigit():
        raise argparse.ArgumentTypeError("expected HOST:PORT, got %r" % text)
    return (host, int(port))


class DelayRelay(asyncio.DatagramProtocol):
    """Forward datagrams between one client and one upstream, delayed.

    A single QUIC client per relay is all the harness needs, so the peer map
    is one slot: whichever address last sent us something that was not the
    upstream is the client. That keeps the relay stateless enough to survive
    the client's connection ID changing mid-flight.
    """

    def __init__(self, upstream, delay_s, loop):
        self.upstream = upstream
        self.delay_s = delay_s
        self.loop = loop
        self.transport = None
        self.client = None
        self.forwarded = 0

    def connection_made(self, transport):
        self.transport = transport

    def datagram_received(self, data, addr):
        # Compare on the resolved tuple: the client and the upstream can only
        # be told apart by address, and a reply from upstream must go back to
        # the client rather than being echoed at upstream again.
        if addr == self.upstream:
            target = self.client
        else:
            self.client = addr
            target = self.upstream
        if target is None:
            return
        self.forwarded += 1
        self.loop.call_later(self.delay_s, self._send, data, target)

    def _send(self, data, target):
        # The transport is closed only at shutdown, and a datagram scheduled
        # just before that would otherwise raise into the event loop.
        if self.transport is not None and not self.transport.is_closing():
            self.transport.sendto(data, target)

    def error_received(self, exc):
        # A UDP ICMP error (upstream not listening yet) is not fatal: the QUIC
        # client will retransmit its initial and the server will be up by then.
        print("udp-delay: %s" % exc, file=sys.stderr)


async def run(listen, upstream, delay_s, ready_fd):
    loop = asyncio.get_running_loop()
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: DelayRelay(upstream, delay_s, loop),
        local_addr=listen,
        family=socket.AF_INET,
    )
    if ready_fd is not None:
        # The harness waits on this byte rather than sleeping: a QUIC client
        # that dials before the relay binds would burn its handshake timeout.
        with open(ready_fd, "w") as handle:
            handle.write("ready\n")
    try:
        await asyncio.get_running_loop().create_future()
    finally:
        transport.close()
        del protocol


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=parse_addr, required=True)
    ap.add_argument("--to", dest="upstream", type=parse_addr, required=True)
    ap.add_argument("--delay-ms", type=float, required=True,
                    help="one-way delay; a round trip pays it twice")
    ap.add_argument("--ready-file", help="write 'ready' here once bound")
    args = ap.parse_args()
    try:
        asyncio.run(run(args.listen, args.upstream, args.delay_ms / 1000.0,
                        args.ready_file))
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
