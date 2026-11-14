"""Hidden check for the py-duration-hours task.

Run with `python3 -I`, so nothing in the workspace can stand in for the standard
library: the workspace goes at the end of the path, only to import the code under test.
"""

import os
import sys
import unittest

sys.path.append(os.getcwd())

from durations import parse_duration  # noqa: E402


class HiddenDurationTest(unittest.TestCase):
    def test_hours(self):
        self.assertEqual(parse_duration("1h"), 3600)
        self.assertEqual(parse_duration("1h30m"), 5400)
        self.assertEqual(parse_duration("2h5m10s"), 7510)
        self.assertEqual(parse_duration("10h"), 36000)
        self.assertEqual(parse_duration("1h0s"), 3600)
        self.assertEqual(parse_duration(" 1H "), 3600)

    def test_minutes_and_seconds_unchanged(self):
        self.assertEqual(parse_duration("90m"), 5400)
        self.assertEqual(parse_duration("45s"), 45)
        self.assertEqual(parse_duration("1m5s"), 65)
        self.assertEqual(parse_duration("0s"), 0)

    def test_rejects_garbage(self):
        for text in ["", "5", "5x", "h", "1m1h", "1h1h", "1h 30m", "-1h", "1.5h"]:
            with self.assertRaises(ValueError, msg=text):
                parse_duration(text)


if __name__ == "__main__":
    unittest.main()
