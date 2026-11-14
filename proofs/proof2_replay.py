#!/usr/bin/env python3
"""Proof 2: answer the game's Tobii DLL with recorded Eye Tracker 5 replies.

Listens on 127.0.0.1:4455, answers each TTP command with the reply a real
tracker gave (fixtures/et5), and replays one recorded gaze frame at 30 Hz once
the DLL subscribes to the gaze stream. No tracker is involved: this shows
whether the DLL accepts the route, not that tracking works.

    python3 proofs/proof2_replay.py [seconds] [--clock raw|monotonic|boottime|device]

Exits by itself after `seconds` (default 60). Every command and reply is
logged, so an unknown command shows up by number.
"""

from __future__ import annotations

import argparse
import asyncio
import pathlib
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from opentobii_bridge import ttp  # noqa: E402

FIXTURES = ROOT / "fixtures" / "et5"
PORT = 4455

CMD_UPGRADE = 1000
CMD_STREAM_START = 1220
CMD_STREAM_STOP = 1230
CMD_UNLOCK_PPU = 1600
STREAM_GAZE = 0x500
STREAM_PRESENCE = 0x504

RECORDED = {
    1000: "init-rsp-1000",
    1200: "init-rsp-1200",
    1330: "init-rsp-1330",
    1400: "init-rsp-1400",
    1420: "init-rsp-1420",
    1430: "init-rsp-1430",
    1490: "init-rsp-1490",
    1650: "init-rsp-1650",
    2110: "init-rsp-2110",
}

CLOCKS = {
    "raw": time.CLOCK_MONOTONIC_RAW,
    "monotonic": time.CLOCK_MONOTONIC,
    "boottime": time.CLOCK_BOOTTIME,
}

T0 = time.monotonic()


def log(*parts: object) -> None:
    print(f"{time.monotonic() - T0:7.2f}s", *parts, flush=True)


def load(name: str) -> ttp.Message:
    return ttp.from_usb_hex((FIXTURES / f"{name}.hex").read_text())


class Replay:
    def __init__(self, clock: str) -> None:
        self.clock = clock
        self.offset_micros: int | None = None
        self.replies = {cmd: load(name).payload for cmd, name in RECORDED.items()}
        self.gaze = load("session1-gaze-frame")
        self.presence = load("init-presence")
        self.connections = 0
        self.gaze_frames = 0
        self.commands: dict[int, int] = {}

    def stamp(self, body: bytes) -> bytes:
        if self.offset_micros is not None:
            # The helper told us the game's counter; follow that clock.
            raw = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW) // 1000
            return ttp.with_timestamp(body, raw + self.offset_micros)
        if self.clock == "device":
            return body
        micros = time.clock_gettime_ns(CLOCKS[self.clock]) // 1000
        return ttp.with_timestamp(body, micros)

    def answer(self, request: ttp.Message) -> ttp.Message:
        if request.id in self.replies:
            return ttp.response(request, self.replies[request.id])
        if request.id == CMD_UNLOCK_PPU:
            # No recording of this one. Garulf/tobii-linux documents the ET5
            # answering "0, 0, 0"; three u32 zeros is our reading of that.
            return ttp.response(request, ttp.payload(ttp.u32(0), ttp.u32(0), ttp.u32(0)))
        if request.id in (CMD_STREAM_START, CMD_STREAM_STOP):
            return ttp.response(request)
        return ttp.response(request, error=ttp.ERROR_UNSUPPORTED)

    async def stream_gaze(self, writer: asyncio.StreamWriter, tag: str) -> None:
        try:
            while True:
                frame = ttp.Message(
                    ttp.MARKER_STREAM, 0, 0, STREAM_GAZE, 0, self.stamp(self.gaze.payload)
                )
                writer.write(frame.encode())
                await writer.drain()
                self.gaze_frames += 1
                await asyncio.sleep(1 / 30)
        except (ConnectionError, asyncio.CancelledError):
            log(tag, "gaze stream ended")

    async def start_presence(self, writer: asyncio.StreamWriter) -> None:
        once = ttp.Message(
            ttp.MARKER_STREAM, 0, 0, STREAM_PRESENCE, 0, self.stamp(self.presence.payload)
        )
        writer.write(once.encode())
        await writer.drain()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.connections += 1
        tag = f"conn#{self.connections}"
        log(tag, "accepted")
        streams: dict[int, asyncio.Task] = {}
        try:
            while True:
                head = await reader.readexactly(ttp.HEADER_LEN)
                length = ttp.decode_header(head)[5]
                request = ttp.decode(head + await reader.readexactly(length))
                self.commands[request.id] = self.commands.get(request.id, 0) + 1
                reply = self.answer(request)
                note = "ok" if reply.error == 0 else f"error {reply.error:#x}"
                detail = ""
                if request.id in (CMD_STREAM_START, CMD_STREAM_STOP):
                    stream = ttp.first_u32(request.payload)
                    detail = f" stream {stream:#x}" if stream is not None else " stream ?"
                log(
                    tag,
                    f"cmd {request.id} seq {request.seq} ({length}B){detail}"
                    f" -> {note}, {len(reply.payload)}B",
                )
                writer.write(reply.encode())
                await writer.drain()
                if request.id == CMD_STREAM_START:
                    if stream == STREAM_GAZE and stream not in streams:
                        streams[stream] = asyncio.create_task(self.stream_gaze(writer, tag))
                    elif stream == STREAM_PRESENCE:
                        await self.start_presence(writer)
                elif request.id == CMD_STREAM_STOP and stream in streams:
                    streams.pop(stream).cancel()
        except asyncio.IncompleteReadError as end:
            log(tag, "closed by peer" + (f" mid-message ({len(end.partial)}B)" if end.partial else ""))
        except ConnectionError as err:
            log(tag, f"connection error: {err}")
        finally:
            for task in streams.values():
                task.cancel()
            writer.close()


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("seconds", nargs="?", type=float, default=60)
    parser.add_argument("--clock", choices=[*CLOCKS, "device"], default="raw")
    args = parser.parse_args()

    replay = Replay(args.clock)
    server = await asyncio.start_server(replay.handle, "127.0.0.1", PORT)
    log(f"listening on 127.0.0.1:{PORT} for {args.seconds:.0f}s, clock={args.clock}")
    async with server:
        await asyncio.sleep(args.seconds)
    counts = ", ".join(f"{cmd}x{n}" for cmd, n in sorted(replay.commands.items())) or "none"
    log(f"done: {replay.connections} connections, {replay.gaze_frames} gaze frames sent")
    log(f"commands seen: {counts}")


if __name__ == "__main__":
    asyncio.run(main())
