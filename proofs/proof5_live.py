#!/usr/bin/env python3
"""Proof 5: live gaze and head pose from a real tracker reach the game.

As proof 3, but the data comes from `tobiid`, the daemon of
Skyggedans/tobii-linux, instead of a recording and a synthetic sweep:

  tobiid socket    raw gaze, presence, head pose and the display area
  127.0.0.1:4455   TTP to the game's DLL; gaze and presence frames are rebuilt
                   from the live samples, the display area is tobiid's
  127.0.0.1:47455  the services pipe, relayed by helper/sesp_relay.exe

Device identity and the other start-up replies still come from the
recordings.

    python3 proofs/proof5_live.py [seconds] [--monitor N] [--socket PATH]

Needs tobiid running with a tracker attached. Start this before the game.
Exits by itself after `seconds` (default 1800).
"""

from __future__ import annotations

import argparse
import asyncio
import pathlib
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "proofs"))

from opentobii_bridge import frames, sesp, smooth, tobiid, ttp  # noqa: E402
from proof2_replay import PORT as TTP_PORT, STREAM_GAZE, STREAM_PRESENCE, Replay, log  # noqa: E402
from proof3_service import RELAY_PORT, Services  # noqa: E402

CMD_DISPLAY_AREA = 1430
STREAMS = tobiid.STREAM_GAZE_RAW | tobiid.STREAM_PRESENCE | tobiid.STREAM_HEAD_POSE
AREA_REQUEST_ID = 1


def monotonic_micros() -> int:
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC) // 1000


def raw_micros() -> int:
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW) // 1000


class LiveReplay(Replay):
    """The TTP side, with gaze and presence pushed from tobiid's samples."""

    def __init__(self) -> None:
        super().__init__("raw")
        self.gaze_template = frames.Template(self.gaze.payload)
        self.presence_template = frames.Template(self.presence.payload)
        self.gaze_writers: set[asyncio.StreamWriter] = set()
        self.presence_writers: set[asyncio.StreamWriter] = set()
        self.presence_status = tobiid.PRESENCE_UNKNOWN
        self.display: frames.DisplayFrame | None = None
        self.presence_frames = 0

    def game_micros(self, host_micros: int | None) -> int:
        """A tobiid host time (CLOCK_MONOTONIC) on the clock the game reads.
        The helper's hello gave the offset from CLOCK_MONOTONIC_RAW."""
        now = monotonic_micros()
        host = now if host_micros is None else host_micros
        if self.offset_micros is None:
            return host
        return host + (raw_micros() - now) + self.offset_micros

    async def stream_gaze(self, writer: asyncio.StreamWriter, tag: str) -> None:
        self.gaze_writers.add(writer)
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            log(tag, "gaze stream ended")
        finally:
            self.gaze_writers.discard(writer)

    async def start_presence(self, writer: asyncio.StreamWriter) -> None:
        self.presence_writers.add(writer)
        if self.presence_status != tobiid.PRESENCE_UNKNOWN:
            self.send(self.presence_writers, STREAM_PRESENCE, self.presence_payload(None))

    def presence_payload(self, host_micros: int | None) -> bytes:
        return frames.presence_frame(
            self.presence_template, self.presence_status, self.game_micros(host_micros)
        )

    def send(self, writers: set[asyncio.StreamWriter], stream: int, payload: bytes) -> None:
        data = ttp.Message(ttp.MARKER_STREAM, 0, 0, stream, 0, payload).encode()
        for writer in list(writers):
            if writer.is_closing():
                writers.discard(writer)
            else:
                writer.write(data)

    def on_gaze(self, raw: tobiid.GazeRaw) -> None:
        if not self.gaze_writers:
            return
        payload = frames.gaze_frame(
            self.gaze_template, raw, self.game_micros(raw.host_micros), self.display
        )
        self.send(self.gaze_writers, STREAM_GAZE, payload)
        self.gaze_frames += 1

    def on_presence(self, presence: tobiid.Presence) -> None:
        changed = presence.status != self.presence_status
        self.presence_status = presence.status
        if changed:
            log(f"tobiid: presence {presence.status}")
        if self.presence_writers and presence.status != tobiid.PRESENCE_UNKNOWN:
            self.send(self.presence_writers, STREAM_PRESENCE,
                      self.presence_payload(presence.host_micros))
            self.presence_frames += 1

    def on_display_area(self, corners) -> None:
        top_left, top_right, bottom_left = corners
        self.replies[CMD_DISPLAY_AREA] = frames.display_area_reply(
            self.replies[CMD_DISPLAY_AREA], top_left, top_right, bottom_left
        )
        self.display = frames.DisplayFrame(top_left, top_right, bottom_left)
        width = abs(top_right[0] - top_left[0])
        height = abs(top_left[1] - bottom_left[1])
        log(f"tobiid: display area {width:.0f} x {height:.0f} mm")


class LiveServices(Services):
    """The services side, with head poses pushed from tobiid's samples."""

    def __init__(self, replay: LiveReplay, monitor_index: int | None,
                 pose_filter: smooth.PoseFilter | None = None) -> None:
        super().__init__(replay, monitor_index)
        self.pose_filter = pose_filter
        self.sequences: dict[int, int] = {}
        self.poses_skipped = 0

    async def send_poses(self, pid: int) -> None:
        # Poses are pushed by on_pose; this task only marks the pid as wanted.
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            pass

    def on_pose(self, pose: tobiid.HeadPose) -> None:
        if not pose.valid:
            # The validity fields' width in the services message is not known
            # yet, so an invalid pose is dropped rather than sent as valid.
            self.poses_skipped += 1
            return
        position, rotation = pose.position_mm, pose.rotation_rad
        if self.pose_filter is not None:
            # Filter every pose, wanted or not, so the state is warm when a
            # game subscribes.
            position, rotation = self.pose_filter(pose.host_micros, position, rotation)
        micros = self.replay.game_micros(pose.host_micros)
        for pid in list(self.posing):
            writer = self.subscribers.get(pid)
            if writer is None or writer.is_closing():
                continue
            sequence = self.sequences[pid] = self.sequences.get(pid, 0) + 1
            writer.write(sesp.frame(
                sesp.head_pose(sequence, micros, position, rotation)
            ))
            self.poses_sent += 1


async def follow_tobiid(path: pathlib.Path, replay: LiveReplay, services: LiveServices) -> None:
    """Stay connected to tobiid, reconnecting every 2 s while it is away."""
    reported = False
    while True:
        try:
            reader, writer = await asyncio.open_unix_connection(str(path))
        except OSError as err:
            if not reported:
                log(f"tobiid: cannot connect to {path} ({err}); retrying every 2 s")
                reported = True
            await asyncio.sleep(2)
            continue
        reported = False
        log(f"tobiid: connected to {path}")
        counts = {"gaze": 0, "pose": 0}
        try:
            writer.write(tobiid.frame(tobiid.subscribe(STREAMS)))
            writer.write(tobiid.frame(
                tobiid.request(AREA_REQUEST_ID, tobiid.KIND_DISPLAY_AREA_GET)
            ))
            await writer.drain()
            pending = b""
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                pending += data
                while True:
                    body, pending = tobiid.unframe(pending)
                    if body is None:
                        break
                    message = tobiid.decode(body)
                    if isinstance(message, tobiid.GazeRaw):
                        counts["gaze"] += 1
                        if counts["gaze"] == 1:
                            log("tobiid: first gaze sample")
                        replay.on_gaze(message)
                    elif isinstance(message, tobiid.HeadPose):
                        counts["pose"] += 1
                        if counts["pose"] == 1:
                            log("tobiid: first head pose")
                        services.on_pose(message)
                    elif isinstance(message, tobiid.Presence):
                        replay.on_presence(message)
                    elif isinstance(message, tobiid.Subscribed):
                        log(f"tobiid: subscribed ok={message.ok}")
                    elif isinstance(message, tobiid.Reply) and message.request_id == AREA_REQUEST_ID:
                        corners = tobiid.display_area(message.payload)
                        if message.status == tobiid.STATUS_OK and corners:
                            replay.on_display_area(corners)
                        else:
                            log(f"tobiid: no display area (status {message.status});"
                                " the recorded one stays in use")
        except (ConnectionError, OSError) as err:
            log(f"tobiid: connection error: {err}")
        finally:
            writer.close()
        log(f"tobiid: disconnected after {counts['gaze']} gaze samples"
            f" and {counts['pose']} head poses")
        await asyncio.sleep(2)


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("seconds", nargs="?", type=float, default=1800)
    parser.add_argument("--monitor", type=int, default=None, help="index of the monitor to report")
    parser.add_argument("--socket", type=pathlib.Path, default=tobiid.socket_path())
    defaults = smooth.Settings()
    parser.add_argument("--no-smooth", action="store_true", help="send head poses unfiltered")
    parser.add_argument("--rot-cutoff", type=float, default=defaults.rotation_min_cutoff,
                        help="rotation filter cutoff at rest, Hz: lower is steadier")
    parser.add_argument("--rot-beta", type=float, default=defaults.rotation_beta,
                        help="how fast the rotation filter opens with speed")
    parser.add_argument("--pos-cutoff", type=float, default=defaults.position_min_cutoff)
    parser.add_argument("--pos-beta", type=float, default=defaults.position_beta)
    args = parser.parse_args()
    pose_filter = None if args.no_smooth else smooth.PoseFilter(smooth.Settings(
        args.rot_cutoff, args.rot_beta, args.pos_cutoff, args.pos_beta
    ))

    replay = LiveReplay()
    services = LiveServices(replay, args.monitor, pose_filter)
    ttp_server = await asyncio.start_server(replay.handle, "127.0.0.1", TTP_PORT)
    relay_server = await asyncio.start_server(services.handle, "127.0.0.1", RELAY_PORT)
    log(f"listening: TTP on {TTP_PORT}, services relay on {RELAY_PORT}, for {args.seconds:.0f}s")
    log("head pose smoothing: " + ("off" if pose_filter is None else
        f"rotation {args.rot_cutoff} Hz beta {args.rot_beta},"
        f" position {args.pos_cutoff} Hz beta {args.pos_beta}"))
    follower = asyncio.create_task(follow_tobiid(args.socket, replay, services))
    async with ttp_server, relay_server:
        await asyncio.sleep(args.seconds)
    follower.cancel()
    log(f"done: {replay.gaze_frames} gaze frames, {replay.presence_frames} presence frames and"
        f" {services.poses_sent} head poses sent to the game;"
        f" {services.poses_skipped} invalid poses dropped")


if __name__ == "__main__":
    asyncio.run(main())
