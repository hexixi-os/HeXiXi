import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_official_matrix import inspect_serial, select_groups


class OfficialMatrixTests(unittest.TestCase):
    start = b"#### OS COMP TEST GROUP START basic-musl ####\r\n"
    end = b"#### OS COMP TEST GROUP END basic-musl ####\r\n"
    result = b"[oscomp] result basic-musl exit=0\r\n"
    shutdown = b"[kernel] Idle process exit with exit_code 0 ...\r\n"

    def inspect(self, serial, rc=0, timeout=False):
        return inspect_serial(serial, "basic-musl", rc, timeout)

    def test_select_groups_without_repeating_boots(self):
        self.assertEqual(len(select_groups([])), 24)
        self.assertEqual(select_groups(["basic-musl", "basic"]), ["basic-musl", "basic-glibc"])
        with self.assertRaises(ValueError):
            select_groups(["basic-typo"])

    def test_extract_only_the_selected_group(self):
        serial = b"boot\n" + self.start + b"case\0\r\n" + self.end + self.result + self.shutdown
        result, body = self.inspect(serial)
        self.assertEqual(result["status"], "complete")
        self.assertEqual(body, "case\n")

    def test_timeout_preserves_partial_output(self):
        result, body = self.inspect(self.start + b"case\r\n", None, True)
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(result["coverage"], "partial")
        self.assertEqual(body, "case\n")

    def test_panic_takes_precedence_over_qemu_error(self):
        result, body = self.inspect(self.start + b"case\n[kernel] PANIC pid=3\n", 1)
        self.assertEqual(result["status"], "panic")
        self.assertEqual(body, "case\n")
        self.assertEqual(result["diagnostics"], ["[kernel] PANIC pid=3"])

    def test_repeated_group_markers_do_not_feed_the_judge(self):
        result, body = self.inspect(self.start * 2 + self.end + self.result + self.shutdown)
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(body, "")

    def test_failed_shell_is_distinct_from_incomplete_group(self):
        result, body = self.inspect(b"[oscomp] result basic-musl exit=-11\n" + self.shutdown)
        self.assertEqual(result["status"], "launcher_error")
        self.assertEqual(result["script_exit"], -11)
        self.assertEqual(body, "")


if __name__ == "__main__":
    unittest.main()
