"""End to end on throwaway ports: a fake tobiid, a fake game DLL on the TTP
side and a fake helper on the services side."""

import asyncio
import pathlib
import struct
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "proofs"))

import proof5_live  # noqa: E402
from opentobii_bridge import frames, sesp, tobiid, ttp  # noqa: E402
from test_frames import GAZE, raw_from  # noqa: E402

AREA = ((-300.0, 340.0, 100.0), (300.0, 340.0, 100.0), (-300.0, 5.0, -2.0))
QPC = 5_000_000_000


def gaze_raw_body(raw: tobiid.GazeRaw, host_micros: int) -> bytes:
    def eye(e: tobiid.RawEye) -> bytes:
        return struct.pack(
            "<12fI", *e.gaze_origin_mm, *e.gaze_origin_in_track_box, *e.gaze_point_mm,
            *e.gaze_point_on_display, e.pupil_diameter_mm, e.status,
        )

    body = bytes([tobiid.TAG_GAZE_RAW]) + struct.pack("<q", raw.device_micros)
    body += eye(raw.left) + eye(raw.right)
    body += struct.pack("<2fI", *raw.combined_on_display, raw.combined_validity)
    for word in (raw.key_0e, raw.key_11, raw.frame_counter,
                 raw.left_origin_flag, raw.right_origin_flag):
        body += struct.pack("<BI", word is not None, word or 0)
    for point in (raw.left_eyeball_center_mm, raw.right_eyeball_center_mm):
        body += struct.pack("<B3f", point is not None, *(point or (0, 0, 0)))
    body += struct.pack("<q", host_micros)
    body += struct.pack("<2fI", *raw.responsive_on_display, raw.responsive_validity)
    return body


async def read_ttp(reader: asyncio.StreamReader) -> ttp.Message:
    head = await reader.readexactly(ttp.HEADER_LEN)
    return ttp.decode(head + await reader.readexactly(ttp.decode_header(head)[5]))


async def read_sesp(reader: asyncio.StreamReader) -> sesp.Parsed:
    pending = b""
    while True:
        buffer, pending = sesp.unframe(pending)
        if buffer is not None:
            return sesp.parse(buffer)
        pending += await reader.read(4096)


def helper_hello(channel: int, slot: int) -> bytes:
    monitor = b"DISPLAY\\TEST\\1"
    return (struct.pack("<4sIIIQI", b"OTB1", channel, 392, slot, QPC, 1)
            + struct.pack("<BH", 1, len(monitor)) + monitor)


class Proof5Test(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.socket = pathlib.Path(self.dir.name) / "tobiid.sock"
        self.daemon_writer: asyncio.Future = asyncio.get_running_loop().create_future()
        self.daemon_got = b""

        async def daemon(reader, writer):
            self.daemon_got = await reader.readexactly(9 + 10)
            writer.write(tobiid.frame(b"\x10\x01"))
            writer.write(tobiid.frame(
                b"\x11" + struct.pack("<IB", proof5_live.AREA_REQUEST_ID, 0)
                + struct.pack("<9f", *AREA[0], *AREA[1], *AREA[2])
            ))
            await writer.drain()
            self.daemon_writer.set_result(writer)
            await reader.read()

        self.daemon = await asyncio.start_unix_server(daemon, str(self.socket))
        self.replay = proof5_live.LiveReplay()
        self.services = proof5_live.LiveServices(self.replay, None)
        self.ttp_server = await asyncio.start_server(self.replay.handle, "127.0.0.1", 0)
        self.relay_server = await asyncio.start_server(self.services.handle, "127.0.0.1", 0)
        self.follower = asyncio.create_task(
            proof5_live.follow_tobiid(self.socket, self.replay, self.services)
        )
        self.to_daemon = await asyncio.wait_for(self.daemon_writer, 2)
        for _ in range(100):
            if self.replay.display is not None:
                break
            await asyncio.sleep(0.01)

    async def asyncTearDown(self):
        self.follower.cancel()
        for server in (self.daemon, self.ttp_server, self.relay_server):
            server.close()
        self.dir.cleanup()

    def port(self, server) -> int:
        return server.sockets[0].getsockname()[1]

    async def test_subscribes_and_asks_for_the_display_area(self):
        streams = tobiid.STREAM_GAZE_RAW | tobiid.STREAM_PRESENCE | tobiid.STREAM_HEAD_POSE
        expected = tobiid.frame(tobiid.subscribe(streams)) + tobiid.frame(
            tobiid.request(proof5_live.AREA_REQUEST_ID, tobiid.KIND_DISPLAY_AREA_GET)
        )
        self.assertEqual(self.daemon_got, expected)

    async def test_the_game_is_told_tobiids_display_area(self):
        reader, writer = await asyncio.open_connection("127.0.0.1", self.port(self.ttp_server))
        writer.write(ttp.Message(ttp.MARKER_COMMAND, 1, 0, 1430, 0, b"").encode())
        reply = await asyncio.wait_for(read_ttp(reader), 2)
        self.assertEqual(tuple(tuple(c) for c in frames.display_area_corners(reply.payload)), AREA)
        writer.close()

    async def test_live_gaze_and_presence_reach_a_subscribed_game(self):
        reader, writer = await asyncio.open_connection("127.0.0.1", self.port(self.ttp_server))
        for seq, stream in ((1, proof5_live.STREAM_PRESENCE), (2, proof5_live.STREAM_GAZE)):
            writer.write(ttp.Message(
                ttp.MARKER_COMMAND, seq, 0, 1220, 0, ttp.payload(ttp.u32(stream))
            ).encode())
            self.assertEqual((await asyncio.wait_for(read_ttp(reader), 2)).error, 0)
        await asyncio.sleep(0.05)

        raw = raw_from(GAZE)
        raw = tobiid.GazeRaw(**{**raw.__dict__, "combined_on_display": (0.125, 0.875),
                                "frame_counter": 99})
        host = proof5_live.monotonic_micros()
        self.to_daemon.write(tobiid.frame(
            bytes([tobiid.TAG_PRESENCE]) + struct.pack("<qB", host, tobiid.PRESENCE_PRESENT)
        ))
        self.to_daemon.write(tobiid.frame(gaze_raw_body(raw, host)))
        await self.to_daemon.drain()

        presence = await asyncio.wait_for(read_ttp(reader), 2)
        self.assertEqual((presence.marker, presence.id), (ttp.MARKER_STREAM, 0x504))
        self.assertEqual(frames.keyed_values(presence.payload)[2], 2)
        gaze = await asyncio.wait_for(read_ttp(reader), 2)
        self.assertEqual((gaze.marker, gaze.id), (ttp.MARKER_STREAM, 0x500))
        values = frames.keyed_values(gaze.payload)
        self.assertEqual(values[0x1C], [0.125, 0.875])
        self.assertEqual(values[0x14], 99)
        self.assertEqual(values[0x01], host)  # no helper hello yet: host clock as is
        self.assertEqual(len(gaze.payload), len(GAZE))
        writer.close()

    async def test_live_head_pose_reaches_the_game_on_the_games_clock(self):
        port = self.port(self.relay_server)
        command_reader, command = await asyncio.open_connection("127.0.0.1", port)
        command.write(helper_hello(sesp.CHANNEL_COMMAND, 1))
        sub_reader, sub = await asyncio.open_connection("127.0.0.1", port)
        sub.write(helper_hello(sesp.CHANNEL_SUBSCRIPTION, 2))
        await sub.drain()
        command.write(sesp.frame(sesp.build(1, sesp.T_HEAD_POSE_START)))
        started = await asyncio.wait_for(read_sesp(command_reader), 2)
        self.assertEqual(started.kind, sesp.T_HEAD_POSE_START_RESPONSE)
        await asyncio.sleep(0.05)

        host = proof5_live.monotonic_micros()
        invalid = struct.pack("<qB6f", host, 0b0001, 0, 0, 0, 0, 0, 0)
        valid = struct.pack("<qB6f", host, 0b1111, 10.0, 20.0, 600.0, 0.1, -0.2, 0.05)
        self.to_daemon.write(tobiid.frame(bytes([tobiid.TAG_HEAD_POSE]) + invalid))
        self.to_daemon.write(tobiid.frame(bytes([tobiid.TAG_HEAD_POSE]) + valid))
        await self.to_daemon.drain()

        pose = await asyncio.wait_for(read_sesp(sub_reader), 2)
        self.assertEqual(pose.kind, sesp.T_HEAD_POSE)
        self.assertEqual(pose.sequence, 1)
        self.assertEqual([pose.payload.f32(i) for i in (2, 3, 4)], [10.0, 20.0, 600.0])
        self.assertAlmostEqual(pose.payload.f32(6), -0.2, places=6)
        micros = (pose.payload.u32(0) << 32) | pose.payload.u32(1)
        # The helper said the game's counter read QPC when it connected.
        self.assertLess(abs(micros - QPC), 2_000_000)
        self.assertEqual(self.services.poses_skipped, 1)
        command.close()
        sub.close()


if __name__ == "__main__":
    unittest.main()
