import pathlib
import struct
import unittest

from opentobii_bridge import ttp

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "fixtures" / "et5"

# The 71 bytes ETS2's Tobii DLL 7.3.0.14 sent to 127.0.0.1:4455 on 2026-11-14.
GAME_UPGRADE_REQUEST = bytes.fromhex(
    "000000510000000100000000000003e8000000000000002f"
    "0000170000002800000009000100000001000100010002000100030001000400010005"
    "000100060001000700010008"
)


def fixture(name: str) -> ttp.Message:
    return ttp.from_usb_hex((FIXTURES / f"{name}.hex").read_text())


class HeaderTest(unittest.TestCase):
    def test_decodes_the_request_the_game_sent(self):
        msg = ttp.decode(GAME_UPGRADE_REQUEST)
        self.assertEqual(msg.marker, ttp.MARKER_COMMAND)
        self.assertEqual(msg.seq, 1)
        self.assertEqual(msg.id, 1000)
        self.assertEqual(len(msg.payload), 47)

    def test_round_trips(self):
        self.assertEqual(ttp.decode(GAME_UPGRADE_REQUEST).encode(), GAME_UPGRADE_REQUEST)

    def test_rejects_a_truncated_payload(self):
        with self.assertRaises(ValueError):
            ttp.decode(GAME_UPGRADE_REQUEST[:-1])

    def test_response_copies_seq_and_command(self):
        reply = ttp.response(ttp.decode(GAME_UPGRADE_REQUEST), error=ttp.ERROR_UNSUPPORTED)
        self.assertEqual(
            (reply.marker, reply.seq, reply.status, reply.id, reply.error),
            (ttp.MARKER_RESPONSE, 1, ttp.STATUS_OK, 1000, ttp.ERROR_UNSUPPORTED),
        )


class TlvTest(unittest.TestCase):
    def test_upgrade_request_lists_nine_versions(self):
        items = list(ttp.tlvs(ttp.decode(GAME_UPGRADE_REQUEST).payload))
        self.assertEqual(len(items), 1)
        typ, value = items[0]
        self.assertEqual(typ, ttp.TYPE_LIST)
        count, *versions = struct.unpack(">10I", value)
        self.assertEqual(count, 9)
        self.assertEqual(versions, [0x10000 + i for i in range(9)])

    def test_recorded_upgrade_reply_picks_the_newest_version(self):
        reply = fixture("init-rsp-1000")
        self.assertEqual(reply.marker, ttp.MARKER_RESPONSE)
        self.assertEqual(ttp.first_u32(reply.payload), 0x10008)

    def test_builds_a_stream_start_payload_that_parses_back(self):
        body = ttp.payload(ttp.u32(0x500), ttp.tlv(ttp.TYPE_LIST, b"\x00\x00\x00\x00"))
        self.assertEqual(ttp.first_u32(body), 0x500)


class TimestampTest(unittest.TestCase):
    def test_gaze_frame_timestamp_is_rewritten_in_place(self):
        frame = fixture("session1-gaze-frame")
        self.assertEqual(frame.id, 0x500)
        at = ttp.timestamp_offset(frame.payload)
        self.assertIsNotNone(at)
        rewritten = ttp.with_timestamp(frame.payload, 123_456_789)
        self.assertEqual(len(rewritten), len(frame.payload))
        self.assertEqual(struct.unpack_from(">Q", rewritten, at)[0], 123_456_789)
        self.assertEqual(rewritten[:at], frame.payload[:at])
        self.assertEqual(rewritten[at + 8 :], frame.payload[at + 8 :])

    def test_presence_frame_has_a_timestamp_too(self):
        self.assertIsNotNone(ttp.timestamp_offset(fixture("init-presence").payload))

    def test_payload_without_a_timestamp_is_left_alone(self):
        body = ttp.payload(ttp.u32(7))
        self.assertEqual(ttp.with_timestamp(body, 1), body)


if __name__ == "__main__":
    unittest.main()
