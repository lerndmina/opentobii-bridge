import pathlib
import tempfile
import unittest

from opentobii_bridge import prefix

REGISTRY = (
    "WINE REGISTRY Version 2\n"
    ";; All keys relative to \\\\Machine\n"
    "\n"
    "#arch=win64\n"
    "\n"
    "[System\\\\ControlSet001\\\\Services\\\\Winmgmt] 1700000000\n"
    '#time=1d0000000000000\n'
    '"Start"=dword:00000003\n'
    "\n"
    "[System\\\\ControlSet001\\\\Services\\\\wuauserv] 1700000000\n"
    '"Start"=dword:00000003\n'
)


class RegistryTextTest(unittest.TestCase):
    def test_add_appends_the_key_once(self):
        once = prefix.add_service(REGISTRY)
        twice = prefix.add_service(once)
        self.assertEqual(once, twice)
        self.assertEqual(once.count(prefix.KEY), 1)

    def test_added_key_is_an_auto_start_own_process_service(self):
        added = prefix.add_service(REGISTRY)
        block = added[added.index(prefix.KEY) :]
        self.assertIn('"Start"=dword:00000002', block)
        self.assertIn('"Type"=dword:00000010', block)
        self.assertIn('"ImagePath"=str(2):"C:\\\\opentobii\\\\sesp_relay.exe"', block)

    def test_add_keeps_every_existing_line(self):
        added = prefix.add_service(REGISTRY)
        for line in REGISTRY.splitlines():
            self.assertIn(line, added.splitlines())

    def test_remove_restores_the_other_keys(self):
        removed = prefix.remove_service(prefix.add_service(REGISTRY))
        self.assertEqual(removed, REGISTRY)

    def test_remove_takes_only_our_key_when_it_sits_in_the_middle(self):
        middle = prefix.add_service(REGISTRY).rstrip("\n") + "\n[System\\\\ControlSet001\\\\Services\\\\Zzz]\n\"Start\"=dword:00000004\n"
        removed = prefix.remove_service(middle)
        self.assertFalse(prefix.has_service(removed))
        self.assertIn("Services\\\\Zzz]", removed)
        self.assertIn('"Start"=dword:00000004', removed)

    def test_remove_is_a_no_op_without_the_key(self):
        self.assertEqual(prefix.remove_service(REGISTRY), REGISTRY)


class InstallTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.prefix = pathlib.Path(self.dir.name) / "pfx"
        self.prefix.mkdir()
        (self.prefix / "system.reg").write_text(REGISTRY)
        self.exe = pathlib.Path(self.dir.name) / "sesp_relay.exe"
        self.exe.write_bytes(b"MZ-not-really")

    def tearDown(self):
        self.dir.cleanup()

    def test_install_then_uninstall_leaves_the_prefix_as_it_was(self):
        prefix.install(self.prefix, self.exe)
        self.assertTrue((self.prefix / "drive_c/opentobii/sesp_relay.exe").is_file())
        self.assertTrue(prefix.has_service((self.prefix / "system.reg").read_text()))
        self.assertEqual((self.prefix / "system.reg.opentobii-backup").read_text(), REGISTRY)

        prefix.uninstall(self.prefix)
        self.assertFalse((self.prefix / "drive_c/opentobii").exists())
        self.assertEqual((self.prefix / "system.reg").read_text(), REGISTRY)

    def test_install_refuses_a_folder_that_is_not_a_prefix(self):
        with self.assertRaises(SystemExit):
            prefix.install(pathlib.Path(self.dir.name) / "nope", self.exe)

    def test_backup_keeps_the_original_across_reinstalls(self):
        prefix.install(self.prefix, self.exe)
        prefix.install(self.prefix, self.exe)
        self.assertEqual((self.prefix / "system.reg.opentobii-backup").read_text(), REGISTRY)


if __name__ == "__main__":
    unittest.main()
