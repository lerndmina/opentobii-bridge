import struct
import unittest

from opentobii_bridge import sesp


class FramingTest(unittest.TestCase):
    def test_frame_starts_with_the_letters_sesp(self):
        self.assertEqual(sesp.frame(b"abcd")[:4], b"sesp")

    def test_frame_carries_size_and_its_check_word(self):
        _, size, check = struct.unpack_from("<III", sesp.frame(b"abcdefgh"))
        self.assertEqual((size, check), (8, 8 ^ 0x70736573))

    def test_unframe_returns_the_buffer_and_the_rest(self):
        data = sesp.frame(b"abcd") + b"tail"
        self.assertEqual(sesp.unframe(data), (b"abcd", b"tail"))

    def test_unframe_waits_for_a_whole_frame(self):
        data = sesp.frame(b"abcdefgh")
        for cut in (0, 5, 12, len(data) - 1):
            self.assertEqual(sesp.unframe(data[:cut]), (None, data[:cut]))

    def test_unframe_rejects_a_bad_check_word(self):
        data = bytearray(sesp.frame(b"abcd"))
        data[8] ^= 1
        with self.assertRaises(ValueError):
            sesp.unframe(bytes(data))


class BufferTest(unittest.TestCase):
    def test_root_fields_read_back(self):
        parsed = sesp.parse(sesp.build(7, sesp.T_HEAD_POSE_START))
        self.assertEqual((parsed.sequence, parsed.kind), (7, sesp.T_HEAD_POSE_START))
        self.assertIsNotNone(parsed.payload)

    def test_every_offset_is_four_byte_aligned(self):
        buf = sesp.display_info_response(3, "DISPLAY\\DEFAULT_MONITOR\\0000&0000")
        self.assertEqual(len(buf) % 4, 0)
        root_at = struct.unpack_from("<I", buf, 0)[0]
        self.assertEqual(root_at % 4, 0)

    def test_absent_fields_read_as_their_default(self):
        payload = sesp.parse(sesp.build(1, sesp.T_STREAM_STATUS_RESPONSE)).payload
        self.assertEqual(payload.u32(0), 0)
        self.assertEqual(payload.u8(9, default=1), 1)
        self.assertIsNone(payload.string(1))

    def test_string_and_scalar_fields_share_a_table(self):
        table = sesp.Table({0: "tet-tcp://127.0.0.1", 1: sesp.U32(42)})
        payload = sesp.parse(sesp.build(2, sesp.T_INITIALIZE, table)).payload
        self.assertEqual(payload.string(0), "tet-tcp://127.0.0.1")
        self.assertEqual(payload.u32(1), 42)


class MessageTest(unittest.TestCase):
    def test_initialize_response_sets_only_the_asked_capabilities(self):
        buf = sesp.initialize_response(5, {sesp.CAP_HEAD_POSE, sesp.CAP_STREAM_STATUS})
        parsed = sesp.parse(buf)
        self.assertEqual((parsed.sequence, parsed.kind), (5, sesp.T_INITIALIZE_RESPONSE))
        flags = [parsed.payload.u8(n) for n in range(1, 16)]
        self.assertEqual(flags, [1, 0, 1] + [0] * 12)
        self.assertEqual(parsed.payload.u32(0), 0)

    def test_head_pose_splits_the_timestamp_into_two_words(self):
        micros = (3 << 32) | 0x89ABCDEF
        payload = sesp.parse(sesp.head_pose(1, micros, (1.5, -2.0, 600.0), (0.1, 0.2, -0.3))).payload
        self.assertEqual((payload.u32(0), payload.u32(1)), (3, 0x89ABCDEF))
        self.assertEqual([payload.f32(n) for n in (2, 3, 4)], [1.5, -2.0, 600.0])
        for n, want in zip((5, 6, 7), (0.1, 0.2, -0.3)):
            self.assertAlmostEqual(payload.f32(n), want, places=6)
        self.assertEqual([payload.u8(n, default=1) for n in (8, 9, 10, 11)], [1, 1, 1, 1])

    def test_display_info_response_carries_the_monitor_id(self):
        monitor = "DISPLAY\\GSM5C7C\\5&1234"
        payload = sesp.parse(sesp.display_info_response(9, monitor)).payload
        self.assertEqual(payload.string(1), monitor)
        self.assertEqual(payload.u32(0), 0)


class RespondTest(unittest.TestCase):
    CAPS = {sesp.CAP_HEAD_POSE, sesp.CAP_DISPLAY_INFO, sesp.CAP_STREAM_STATUS}

    def answer(self, kind: int, payload: sesp.Table | None = None) -> sesp.Parsed:
        request = sesp.parse(sesp.build(11, kind, payload))
        return sesp.parse(sesp.respond(request, self.CAPS, "DISPLAY\\X\\1"))

    def test_each_request_gets_its_response_type_and_sequence(self):
        pairs = {
            sesp.T_INITIALIZE: sesp.T_INITIALIZE_RESPONSE,
            sesp.T_HEAD_POSE_START: sesp.T_HEAD_POSE_START_RESPONSE,
            sesp.T_HEAD_POSE_STOP: sesp.T_HEAD_POSE_STOP_RESPONSE,
            sesp.T_DISPLAY_INFO: sesp.T_DISPLAY_INFO_RESPONSE,
            sesp.T_STREAM_STATUS: sesp.T_STREAM_STATUS_RESPONSE,
            sesp.T_STATISTICS: sesp.T_STATISTICS_RESPONSE,
        }
        for request_kind, response_kind in pairs.items():
            reply = self.answer(request_kind)
            self.assertEqual((reply.sequence, reply.kind), (11, response_kind))
            self.assertEqual(reply.payload.u32(0), 0)

    def test_unknown_request_is_refused_as_not_supported(self):
        reply = self.answer(0x25)
        self.assertEqual(reply.kind, sesp.T_INVALID_OPERATION)
        self.assertEqual(reply.payload.u32(0), sesp.ERROR_NOT_SUPPORTED)


if __name__ == "__main__":
    unittest.main()
