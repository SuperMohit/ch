import unittest

from durations import parse_duration


class ParseDurationTest(unittest.TestCase):
    def test_seconds(self):
        self.assertEqual(parse_duration("45s"), 45)

    def test_minutes_and_seconds(self):
        self.assertEqual(parse_duration("2m30s"), 150)
        self.assertEqual(parse_duration(" 10M "), 600)

    def test_rejects_garbage(self):
        for text in ["", "10", "10x", "1s1m", "1m1m", "1m 5s"]:
            with self.assertRaises(ValueError, msg=text):
                parse_duration(text)


if __name__ == "__main__":
    unittest.main()
