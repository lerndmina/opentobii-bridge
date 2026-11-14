import struct
import unittest

from opentobii_bridge import tobiid


def eye_bytes(seed: float, status: int) -> bytes:
    return struct.pack("<12fI", *[seed + i for i in range(12)], status)


def gaze_raw_body(tail: bytes = b"") -> bytes:
    body = bytes([tobiid.TAG_GAZE_RAW]) + struct.pack("<q", 1_000_000)
    body += eye_bytes(100.0, 0) + eye_bytes(200.0, 4)
    body += struct.pack("<2fI", 0.5, 0.25, 1)
    body += struct.pack("<BI", 0, 0)       # key 0x0e absent
    body += struct.pack("<BI", 1, 4)       # key 0x11
    body += struct.pack("<BI", 1, 777)     # frame counter
    body += struct.pack("<BI", 1, 1)       # left origin flag
    body += struct.pack("<BI", 1, 0)       # right origin flag
    body += struct.pack("<B3f", 1, 1.0, 2.0, 3.0)
    body += struct.pack("<B3f", 0, 0.0, 0.0, 0.0)
    return body + tail


class FramingTest(unittest.TestCase):
    def test_frame_round_trip_and_partial_input(self):
        wire = tobiid.frame(b"abc") + tobiid.frame(b"de")
        self.assertEqual(tobiid.unframe(wire[:5]), (None, wire[:5]))
        body, rest = tobiid.unframe(wire)
        self.assertEqual(body, b"abc")
        self.assertEqual(tobiid.unframe(rest), (b"de", b""))

    def test_subscribe_and_request_bodies(self):
        mask = tobiid.STREAM_GAZE_RAW | tobiid.STREAM_HEAD_POSE
        self.assertEqual(tobiid.subscribe(mask), b"\x01\x00\x03\x00\x00")
        self.assertEqual(
            tobiid.request(7, tobiid.KIND_DISPLAY_AREA_GET), b"\x03\x07\x00\x00\x00\x03"
        )


class DecodeTest(unittest.TestCase):
    def test_subscribed_and_reply(self):
        self.assertEqual(tobiid.decode(b"\x10\x01"), tobiid.Subscribed(True))
        self.assertEqual(
            tobiid.decode(b"\x11\x07\x00\x00\x00\x00xyz"), tobiid.Reply(7, 0, b"xyz")
        )

    def test_presence(self):
        body = bytes([tobiid.TAG_PRESENCE]) + struct.pack("<qB", 42, tobiid.PRESENCE_PRESENT)
        self.assertEqual(tobiid.decode(body), tobiid.Presence(42, 2))

    def test_head_pose_flags(self):
        body = bytes([tobiid.TAG_HEAD_POSE]) + struct.pack(
            "<qB6f", 5, 0b1011, 1.0, 2.0, 600.0, 0.1, 0.2, 0.3
        )
        pose = tobiid.decode(body)
        self.assertEqual(pose.host_micros, 5)
        self.assertTrue(pose.position_valid)
        self.assertEqual(pose.rotation_valid, (True, False, True))
        self.assertFalse(pose.valid)
        self.assertEqual(pose.position_mm, (1.0, 2.0, 600.0))
        self.assertAlmostEqual(pose.rotation_rad[1], 0.2, places=6)

    def test_gaze_raw_without_the_tail(self):
        raw = tobiid.decode(gaze_raw_body())
        self.assertEqual(raw.device_micros, 1_000_000)
        self.assertEqual(raw.left.gaze_origin_mm, (100.0, 101.0, 102.0))
        self.assertEqual(raw.left.gaze_point_on_display, (109.0, 110.0))
        self.assertEqual(raw.left.pupil_diameter_mm, 111.0)
        self.assertEqual((raw.left.status, raw.right.status), (0, 4))
        self.assertEqual(raw.combined_on_display, (0.5, 0.25))
        self.assertIsNone(raw.key_0e)
        self.assertEqual(raw.frame_counter, 777)
        self.assertEqual((raw.left_origin_flag, raw.right_origin_flag), (1, 0))
        self.assertEqual(raw.left_eyeball_center_mm, (1.0, 2.0, 3.0))
        self.assertIsNone(raw.right_eyeball_center_mm)
        self.assertIsNone(raw.host_micros)
        self.assertIsNone(raw.responsive_on_display)

    def test_gaze_raw_with_host_time_and_responsive_point(self):
        tail = struct.pack("<q", 2_000_000) + struct.pack("<2fI", 0.75, 0.5, 1)
        raw = tobiid.decode(gaze_raw_body(tail))
        self.assertEqual(raw.host_micros, 2_000_000)
        self.assertEqual(raw.responsive_on_display, (0.75, 0.5))
        self.assertEqual(raw.responsive_validity, 1)

    def test_short_and_unknown_bodies_decode_to_none(self):
        self.assertIsNone(tobiid.decode(b""))
        self.assertIsNone(tobiid.decode(b"\x7f\x00"))
        self.assertIsNone(tobiid.decode(gaze_raw_body()[:50]))
        self.assertIsNone(tobiid.decode(bytes([tobiid.TAG_HEAD_POSE]) + b"\x00" * 10))

    def test_display_area(self):
        payload = struct.pack("<9f", -300, 340, 100, 300, 340, 100, -300, 5, -2)
        self.assertEqual(
            tobiid.display_area(payload),
            ((-300.0, 340.0, 100.0), (300.0, 340.0, 100.0), (-300.0, 5.0, -2.0)),
        )
        self.assertIsNone(tobiid.display_area(payload[:20]))


if __name__ == "__main__":
    unittest.main()
