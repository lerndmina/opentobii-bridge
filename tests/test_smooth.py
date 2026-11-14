import math
import random
import statistics
import unittest

from opentobii_bridge import smooth

DT = 0.0302  # the tracker's frame interval, seconds


class OneEuroTest(unittest.TestCase):
    def test_first_sample_passes_through(self):
        self.assertEqual(smooth.OneEuro(1.0, 0.0)(5.0, 10.0), 5.0)

    def test_a_still_noisy_signal_is_steadied(self):
        rng = random.Random(1)
        f = smooth.OneEuro(0.8, 0.1)
        raw = [10.0 + rng.gauss(0, 0.14) for _ in range(600)]
        out = [f(v, i * DT) for i, v in enumerate(raw)]
        self.assertLess(statistics.pstdev(out[100:]), statistics.pstdev(raw[100:]) / 2.5)
        self.assertAlmostEqual(statistics.mean(out[100:]), 10.0, delta=0.05)

    def test_a_steady_turn_is_followed_with_little_lag(self):
        f = smooth.OneEuro(0.8, 0.1)
        speed = 40.0  # degrees per second
        out = 0.0
        for i in range(60):
            out = f(speed * i * DT, i * DT)
        lag_seconds = (speed * 59 * DT - out) / speed
        self.assertLess(lag_seconds, 0.06)

    def test_a_long_gap_or_a_clock_going_back_restarts_the_filter(self):
        f = smooth.OneEuro(0.8, 0.0)
        f(0.0, 0.0)
        f(0.0, DT)
        self.assertEqual(f(50.0, 5.0), 50.0)
        self.assertEqual(f(20.0, 1.0), 20.0)

    def test_not_a_number_does_not_poison_the_state(self):
        f = smooth.OneEuro(0.8, 0.0)
        f(3.0, 0.0)
        self.assertEqual(f(float("nan"), DT), 3.0)
        self.assertAlmostEqual(f(3.0, 2 * DT), 3.0)

    def test_bad_settings_are_refused(self):
        with self.assertRaises(ValueError):
            smooth.OneEuro(0.0, 0.1)
        with self.assertRaises(ValueError):
            smooth.OneEuro(1.0, -1.0)


class PoseFilterTest(unittest.TestCase):
    def test_filters_rotation_in_degrees_and_returns_radians(self):
        rng = random.Random(2)
        f = smooth.PoseFilter()
        yaw = []
        for i in range(400):
            noisy = math.radians(5.0 + rng.gauss(0, 0.14))
            _, rotation = f(int(i * DT * 1e6), (0.0, 0.0, 800.0), (0.0, noisy, 0.0))
            yaw.append(math.degrees(rotation[1]))
        self.assertAlmostEqual(statistics.mean(yaw[100:]), 5.0, delta=0.05)
        self.assertLess(statistics.pstdev(yaw[100:]), 0.06)

    def test_position_keeps_its_value(self):
        f = smooth.PoseFilter()
        position = (0.0, 0.0, 0.0)
        for i in range(200):
            position, _ = f(int(i * DT * 1e6), (-10.0, 20.0, 800.0), (0.0, 0.0, 0.0))
        for got, want in zip(position, (-10.0, 20.0, 800.0)):
            self.assertAlmostEqual(got, want, places=6)


if __name__ == "__main__":
    unittest.main()
