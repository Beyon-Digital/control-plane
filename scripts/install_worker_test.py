import importlib.util
import pathlib
import unittest
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location("install_worker", pathlib.Path(__file__).with_name("install-worker.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class ServiceConfigurationTests(unittest.TestCase):
    def test_systemd_arguments_cannot_add_directives_or_expand_specifiers(self):
        unit = installer.systemd_unit(["/bin/worker", "/a path/100%done", "evil\nExecStart=/bin/other"], {})
        self.assertEqual(unit.count("\nExecStart="), 1)
        self.assertIn("100%%done", unit)
        self.assertIn(r"evil\nExecStart=", unit)

    def test_windows_task_uses_interactive_identity_without_a_password(self):
        xml = installer.task_xml([r"C:\Program Files\Control Plane\host.exe", "serve", "--workspace-root", r"C:\A & B"], "S-1-5-21-123")
        root = ET.fromstring(xml)
        ns = {"t": "http://schemas.microsoft.com/windows/2004/02/mit/task"}
        self.assertEqual(root.find("t:Principals/t:Principal/t:LogonType", ns).text, "InteractiveToken")
        self.assertNotIn("Password", xml)
        self.assertIn("&amp;", xml)


if __name__ == "__main__":
    unittest.main()
