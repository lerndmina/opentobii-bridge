#!/usr/bin/env python3
"""Proof 3: the Linux side of both routes the game's Tobii DLL uses.

  127.0.0.1:4455   TTP, answered from recordings (as proof 2)
  127.0.0.1:47455  the services pipe, relayed by helper/sesp_relay.exe from
                   inside the game's prefix

The services side answers initialize, display info and head pose start, and
then sends a synthetic head pose: a slow left-right yaw sweep. No tracker is
involved.

    python3 proofs/proof3_service.py [seconds] [--monitor N]

Start it before the game. Exits by itself after `seconds` (default 600).
"""

from __future__ import annotations

import argparse
import asyncio
import math
import pathlib
import struct
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "proofs"))

from opentobii_bridge import sesp  # noqa: E402
from proof2_replay import PORT as TTP_PORT, Replay, log  # noqa: E402

RELAY_PORT = 47455
HELLO_FIXED = struct.Struct("<4sIIIQI")
CAPABILITIES = {sesp.CAP_HEAD_POSE, sesp.CAP_DISPLAY_INFO, sesp.CAP_STREAM_STATUS}
KIND_NAMES = {
    sesp.T_INITIALIZE: "initialize",
    sesp.T_HEAD_POSE_START: "head pose start",
    sesp.T_HEAD_POSE_STOP: "head pose stop",
    sesp.T_DISPLAY_INFO: "display info",
    sesp.T_STREAM_STATUS: "stream status",
    sesp.T_STATISTICS: "statistics",
}


def raw_micros() -> int:
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW) // 1000


class Services:
    def __init__(self, replay: Replay, monitor_index: int | None) -> None:
        self.replay = replay
        self.monitor_index = monitor_index
        self.monitor_id = ""
        self.subscribers: dict[int, asyncio.StreamWriter] = {}
        self.posing: dict[int, asyncio.Task] = {}
        self.poses_sent = 0
        self.requests: dict[int, int] = {}

    async def read_hello(self, reader: asyncio.StreamReader) -> tuple[int, int, int]:
        magic, channel, pid, slot, qpc, count = HELLO_FIXED.unpack(
            await reader.readexactly(HELLO_FIXED.size)
        )
        if magic != b"OTB1":
            raise ValueError(f"unexpected hello {magic!r}")
        monitors = []
        for _ in range(count):
            primary, length = struct.unpack("<BH", await reader.readexactly(3))
            monitors.append((bool(primary), (await reader.readexactly(length)).decode()))
        self.replay.offset_micros = qpc - raw_micros()
        if monitors:
            if self.monitor_index is not None and self.monitor_index < len(monitors):
                chosen = monitors[self.monitor_index][1]
            else:
                chosen = next((name for primary, name in monitors if primary), monitors[0][1])
            if chosen != self.monitor_id:
                for i, (primary, name) in enumerate(monitors):
                    log(f"  monitor {i}{' (primary)' if primary else ''}: {name}")
                log(f"  display id sent to the game: {chosen}")
            self.monitor_id = chosen
        return channel, pid, slot

    async def send_poses(self, pid: int) -> None:
        sequence = 0
        try:
            while True:
                writer = self.subscribers.get(pid)
                if writer is not None:
                    sequence += 1
                    yaw = 0.35 * math.sin(2 * math.pi * time.monotonic() / 4.0)
                    micros = raw_micros() + (self.replay.offset_micros or 0)
                    writer.write(
                        sesp.frame(sesp.head_pose(sequence, micros, (0.0, 0.0, 600.0), (0.0, yaw, 0.0)))
                    )
                    await writer.drain()
                    self.poses_sent += 1
                await asyncio.sleep(1 / 30)
        except (ConnectionError, asyncio.CancelledError):
            pass

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        tag = "sesp"
        pid = 0
        try:
            channel, pid, slot = await self.read_hello(reader)
            tag = f"sesp slot {slot} pid {pid}"
            if channel == sesp.CHANNEL_SUBSCRIPTION:
                log(tag, "subscription channel open")
                self.subscribers[pid] = writer
                while await reader.read(4096):
                    pass
                return
            log(tag, f"command channel open (channel {channel:#x})")
            pending = b""
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                pending += data
                while True:
                    buffer, pending = sesp.unframe(pending)
                    if buffer is None:
                        break
                    request = sesp.parse(buffer)
                    self.requests[request.kind] = self.requests.get(request.kind, 0) + 1
                    name = KIND_NAMES.get(request.kind, f"type {request.kind:#x}")
                    extra = ""
                    if request.kind == sesp.T_INITIALIZE and request.payload is not None:
                        extra = f" url={request.payload.string(0)!r} sub={request.payload.u32(1)}"
                    log(tag, f"request {name} seq {request.sequence}{extra}")
                    writer.write(sesp.frame(sesp.respond(request, CAPABILITIES, self.monitor_id)))
                    await writer.drain()
                    if request.kind == sesp.T_HEAD_POSE_START and pid not in self.posing:
                        self.posing[pid] = asyncio.create_task(self.send_poses(pid))
                    elif request.kind == sesp.T_HEAD_POSE_STOP and pid in self.posing:
                        self.posing.pop(pid).cancel()
        except (asyncio.IncompleteReadError, ConnectionError, ValueError) as err:
            log(tag, f"ended: {err!r}")
        finally:
            if self.subscribers.get(pid) is writer:
                del self.subscribers[pid]
            writer.close()
            log(tag, "closed")


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("seconds", nargs="?", type=float, default=600)
    parser.add_argument("--monitor", type=int, default=None, help="index of the monitor to report")
    args = parser.parse_args()

    replay = Replay("raw")
    services = Services(replay, args.monitor)
    ttp_server = await asyncio.start_server(replay.handle, "127.0.0.1", TTP_PORT)
    relay_server = await asyncio.start_server(services.handle, "127.0.0.1", RELAY_PORT)
    log(f"listening: TTP on {TTP_PORT}, services relay on {RELAY_PORT}, for {args.seconds:.0f}s")
    async with ttp_server, relay_server:
        await asyncio.sleep(args.seconds)
    commands = ", ".join(f"{c}x{n}" for c, n in sorted(replay.commands.items())) or "none"
    requests = ", ".join(
        f"{KIND_NAMES.get(k, hex(k))}x{n}" for k, n in sorted(services.requests.items())
    ) or "none"
    log(f"done: {replay.gaze_frames} gaze frames and {services.poses_sent} head poses sent")
    log(f"TTP commands: {commands}")
    log(f"services requests: {requests}")


if __name__ == "__main__":
    asyncio.run(main())
