import pathlib
import struct
import unittest

from opentobii_bridge import frames, tobiid, ttp

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "fixtures" / "et5"


def load(name: str) -> bytes:
    return ttp.from_usb_hex((FIXTURES / f"{name}.hex").read_text()).payload


GAZE = load("session1-gaze-frame")
AREA = load("init-rsp-1430")
PRESENT = load("init-presence")
AWAY = load("session2-presence-away")


def raw_from(payload: bytes) -> tobiid.GazeRaw:
    """The raw sample `tobiid` would send for a recorded gaze frame."""
    v = frames.keyed_values(payload)

    def eye(k: frames.EyeKeys) -> tobiid.RawEye:
        return tobiid.RawEye(
            tuple(v[k.origin_mm]), tuple(v[k.origin_in_track_box]), tuple(v[k.gaze_point_mm]),
            tuple(v[k.gaze_point_on_display]), v[k.pupil], v[k.status],
        )

    return tobiid.GazeRaw(
        device_micros=v[1], left=eye(frames.LEFT), right=eye(frames.RIGHT),
        combined_on_display=tuple(v[0x1C]), combined_validity=v[0x1B],
        key_0e=None, key_11=v[0x11], frame_counter=v[0x14],
        left_origin_flag=v[0x16], right_origin_flag=v[0x15],
        left_eyeball_center_mm=tuple(v[0x17]), right_eyeball_center_mm=tuple(v[0x18]),
        host_micros=None, responsive_on_display=tuple(v[0x20]), responsive_validity=v[0x1F],
    )


def display() -> frames.DisplayFrame:
    return frames.DisplayFrame(*frames.display_area_corners(AREA))


class KeyedSlotsTest(unittest.TestCase):
    def test_finds_every_key_of_the_recorded_gaze_frame(self):
        slots = frames.keyed_slots(GAZE)
        self.assertEqual(sorted(slots), list(range(0x01, 0x0E)) + [0x11] + list(range(0x14, 0x2D)))
        self.assertEqual(len(slots[0x02]), 3)
        self.assertEqual(len(slots[0x1C]), 2)
        self.assertEqual(len(slots[0x07]), 1)

    def test_decodes_known_values(self):
        v = frames.keyed_values(GAZE)
        self.assertEqual(v[0x14], 43780)
        self.assertAlmostEqual(v[0x06], 6.24736, places=4)
        self.assertAlmostEqual(v[0x02][2], 673.5288, places=3)
        self.assertAlmostEqual(v[0x1C][0], 0.58729, places=4)


class GazeFrameTest(unittest.TestCase):
    def test_refilling_the_recording_with_its_own_values_changes_nothing_else(self):
        template = frames.Template(GAZE)
        rebuilt = frames.gaze_frame(template, raw_from(GAZE), 9613320391, display())
        self.assertEqual(len(rebuilt), len(GAZE))
        before, after = frames.keyed_values(GAZE), frames.keyed_values(rebuilt)
        for key in before:
            if key in (0x22, 0x24):
                continue
            self.assertEqual(after[key], before[key], hex(key))

    def test_display_frame_origins_match_what_the_tracker_sent(self):
        rebuilt = frames.gaze_frame(frames.Template(GAZE), raw_from(GAZE), 1, display())
        before, after = frames.keyed_values(GAZE), frames.keyed_values(rebuilt)
        for key in (0x22, 0x24):
            for a, b in zip(after[key], before[key]):
                self.assertAlmostEqual(a, b, places=3)

    def test_live_values_land_in_the_frame(self):
        raw = raw_from(GAZE)
        left = tobiid.RawEye((1.0, 2.0, 600.0), (0.5, 0.5, 0.5), (10.0, 20.0, 30.0),
                             (0.25, 0.75), 3.5, 4)
        raw = tobiid.GazeRaw(**{**raw.__dict__, "left": left, "combined_on_display": (0.125, 0.875),
                                "combined_validity": 0, "frame_counter": 7})
        v = frames.keyed_values(frames.gaze_frame(frames.Template(GAZE), raw, 123456789, display()))
        self.assertEqual(v[0x01], 123456789)
        self.assertEqual(v[0x02], [1.0, 2.0, 600.0])
        self.assertEqual(v[0x05], [0.25, 0.75])
        self.assertEqual(v[0x06], 3.5)
        self.assertEqual(v[0x07], 4)
        self.assertEqual(v[0x1D], 0)  # a lost eye's gaze point is not valid
        self.assertEqual(v[0x1E], 1)
        self.assertEqual(v[0x1C], [0.125, 0.875])
        self.assertEqual(v[0x1B], 0)
        self.assertEqual(v[0x14], 7)

    def test_without_a_display_area_the_display_frame_origin_is_marked_invalid(self):
        v = frames.keyed_values(frames.gaze_frame(frames.Template(GAZE), raw_from(GAZE), 1, None))
        self.assertEqual(v[0x21], 0)
        self.assertEqual(v[0x23], 0)

    def test_a_missing_responsive_point_is_marked_invalid(self):
        raw = tobiid.GazeRaw(**{**raw_from(GAZE).__dict__, "responsive_on_display": None})
        v = frames.keyed_values(frames.gaze_frame(frames.Template(GAZE), raw, 1, display()))
        self.assertEqual(v[0x1F], 0)

    def test_not_a_number_never_reaches_the_frame(self):
        raw = raw_from(GAZE)
        nan = float("nan")
        raw = tobiid.GazeRaw(**{**raw.__dict__, "combined_on_display": (nan, nan)})
        v = frames.keyed_values(frames.gaze_frame(frames.Template(GAZE), raw, 1, display()))
        self.assertEqual(v[0x1C], frames.keyed_values(GAZE)[0x1C])


class PresenceTest(unittest.TestCase):
    def test_away_from_the_present_template_matches_the_recorded_away_frame(self):
        away = frames.keyed_values(AWAY)
        rebuilt = frames.presence_frame(frames.Template(PRESENT), tobiid.PRESENCE_AWAY, away[1])
        self.assertEqual(rebuilt, AWAY)


class DisplayAreaTest(unittest.TestCase):
    def test_round_trip(self):
        corners = ((-300.0, 340.0, 100.0), (300.0, 340.0, 100.0), (-300.0, 5.0, -2.0))
        reply = frames.display_area_reply(AREA, *corners)
        self.assertEqual(len(reply), len(AREA))
        self.assertEqual(tuple(tuple(c) for c in frames.display_area_corners(reply)), corners)
        # Whatever follows the corners is left as recorded.
        self.assertEqual(reply[-14:], AREA[-14:])

    def test_display_frame_axes(self):
        d = frames.DisplayFrame((-300, 340, 0), (300, 340, 0), (-300, 0, 0))
        self.assertEqual(d.from_tracker((0, 170, 0)), (0, 0, 0))
        self.assertEqual(d.from_tracker((10, 180, 600)), (10, 10, 600))


if __name__ == "__main__":
    unittest.main()
