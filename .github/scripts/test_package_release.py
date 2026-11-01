"""Release signing decisions fail closed and do not alter the source config."""
import unittest
from package_release import configure

class SigningTests(unittest.TestCase):
    def test_missing_keys_skip_each_signing_system_independently(self):
        config = {"macos": {"signingIdentity": "old"}}
        result, apple, minisign = configure(config, {"MINISIGN_KEY": "key"}, "Darwin")
        self.assertFalse(apple)
        self.assertTrue(minisign)
        self.assertNotIn("signingIdentity", result["macos"])
        self.assertEqual(config["macos"]["signingIdentity"], "old")

    def test_complete_apple_credentials_enable_signing_only_on_macos(self):
        environment = dict.fromkeys(("APPLE_CERTIFICATE", "APPLE_SIGNING_IDENTITY", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"), "configured")
        config, apple, minisign = configure({}, environment, "Darwin")
        self.assertTrue(apple)
        self.assertFalse(minisign)
        self.assertEqual(config["macos"]["signingIdentity"], "configured")
        self.assertFalse(configure({}, environment, "Linux")[1])

if __name__ == "__main__":
    unittest.main()
