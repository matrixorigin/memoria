"""Offline installer supply-chain regression tests using a fake curl command.

Run from the repository root: python3 scripts/test_install_security.py -v
Every case executes the actual POSIX installer; no network or real binaries are involved.
"""

import hashlib
import json
import os
import platform
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

INSTALLER = Path(__file__).resolve().with_name("install.sh")
CURL_STUB = '''#!/usr/bin/env python3
import json
import os
import shutil
import sys

args = sys.argv[1:]
url = args[-1]
with open(os.environ["CURL_CALLS"], "a", encoding="utf-8") as log:
    log.write(json.dumps({"url": url, "args": args}) + "\\n")

if "-o" not in args:
    sys.stderr.write("curl mock missing -o\\n")
    sys.exit(2)
out = args[args.index("-o") + 1]
if url.endswith("/SHA256SUMS.txt"):
    if os.environ.get("MOCK_SUM_FAIL") == "1":
        sys.exit(22)
    if os.environ.get("MOCK_SUM_CORRUPT") == "1":
        with open(out, "w", encoding="utf-8") as file:
            file.write("0" * 64 + "  " + os.environ["ASSET_NAME"] + "\\n")
        sys.exit(0)
    if url.startswith("https://github.com/"):
        shutil.copyfile(os.environ["SUM_FILE"], out)
    else:
        # An attacker-controlled mirror could serve a matching forged checksum.
        shutil.copyfile(os.environ["MIRROR_SUM_FILE"], out)
    sys.exit(0)

if url.endswith("/" + os.environ["ASSET_NAME"]):
    if url.startswith("https://github.com/") and os.environ.get("MOCK_DIRECT_FAIL") == "1":
        sys.exit(22)
    if url.startswith("https://mirror.example/"):
        shutil.copyfile(os.environ["MIRROR_ARCHIVE"], out)
    else:
        shutil.copyfile(os.environ["GOOD_ARCHIVE"], out)
    sys.exit(0)

sys.stderr.write("unexpected curl URL: " + url + "\\n")
sys.exit(2)
'''


def _target() -> str:
    arch = platform.machine().lower()
    arch = "x86_64" if arch in ("x86_64", "amd64") else "aarch64"
    os_name = "apple-darwin" if platform.system() == "Darwin" else "unknown-linux-musl"
    return f"{arch}-{os_name}"


class InstallerSecurityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="memoria-install-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.mock_bin = self.root / "bin"
        self.destination = self.root / "install"
        self.mock_bin.mkdir()
        self.destination.mkdir()
        (self.mock_bin / "curl").write_text(CURL_STUB, encoding="utf-8")
        (self.mock_bin / "curl").chmod(0o755)
        self.calls_file = self.root / "curl-calls.jsonl"
        self.calls_file.touch()
        self.archive = self.root / "good.tar.gz"
        self.bad_archive = self.root / "bad.tar.gz"
        self.asset = f"memoria-{_target()}.tar.gz"
        self._make_archive(self.archive, b"#!/bin/sh\necho verified-memoria\n")
        self._make_archive(self.bad_archive, b"#!/bin/sh\necho tampered-memoria\n")
        self.sums_file = self.root / "SHA256SUMS.txt"
        self.sums_file.write_text(
            f"{hashlib.sha256(self.archive.read_bytes()).hexdigest()}  {self.asset}\n",
            encoding="utf-8",
        )
        self.mirror_sums_file = self.root / "mirror-SHA256SUMS.txt"
        self.mirror_sums_file.write_text(
            f"{hashlib.sha256(self.bad_archive.read_bytes()).hexdigest()}  {self.asset}\n",
            encoding="utf-8",
        )

    def _make_archive(self, path: Path, payload: bytes) -> None:
        binary = self.root / "memoria"
        binary.write_bytes(payload)
        binary.chmod(0o755)
        with tarfile.open(path, "w:gz") as archive:
            archive.add(binary, arcname="memoria")

    def _execute(self, *, mirror: bool = False, direct_fail: bool = False,
                 sum_fail: bool = False, sum_corrupt: bool = False,
                 malicious_mirror: bool = False, dry_run: bool = False):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.mock_bin}{os.pathsep}{env['PATH']}",
            "CURL_CALLS": str(self.calls_file),
            "GOOD_ARCHIVE": str(self.archive),
            "MIRROR_ARCHIVE": str(self.bad_archive if malicious_mirror else self.archive),
            "SUM_FILE": str(self.sums_file),
            "MIRROR_SUM_FILE": str(self.mirror_sums_file),
            "ASSET_NAME": self.asset,
            "MOCK_DIRECT_FAIL": "1" if direct_fail else "0",
            "MOCK_SUM_FAIL": "1" if sum_fail else "0",
            "MOCK_SUM_CORRUPT": "1" if sum_corrupt else "0",
            "MEMORIA_NO_TELEMETRY": "1",
        })
        if mirror:
            env["MEMORIA_GHPROXY"] = "https://mirror.example"
        else:
            env.pop("MEMORIA_GHPROXY", None)
        cmd = ["sh", str(INSTALLER), "-v", "v0.0-test", "-y", "-d", str(self.destination)]
        if dry_run:
            cmd.append("-n")
        result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=15)
        calls = [json.loads(s) for s in self.calls_file.read_text().splitlines() if s]
        return result, calls

    def test_official_binary_and_official_checksum_install(self) -> None:
        response, calls = self._execute()
        self.assertEqual(response.returncode, 0, response.stderr)
        self.assertIn("verified-memoria", subprocess.check_output([self.destination / "memoria"], text=True))
        self.assertTrue(all(x["url"].startswith("https://github.com/") for x in calls))
        self.assertTrue(any(x["url"].endswith("SHA256SUMS.txt") for x in calls))

    def test_no_implicit_third_party_fallback(self) -> None:
        response, calls = self._execute(direct_fail=True)
        self.assertNotEqual(response.returncode, 0)
        self.assertFalse((self.destination / "memoria").exists())
        self.assertTrue(all(x["url"].startswith("https://github.com/") for x in calls))

    def test_opt_in_mirror_fetches_binary_but_official_checksum(self) -> None:
        response, calls = self._execute(mirror=True, direct_fail=True)
        self.assertEqual(response.returncode, 0, response.stderr)
        self.assertTrue(any(x["url"].startswith("https://mirror.example/") for x in calls))
        checksum_calls = [x for x in calls if x["url"].endswith("SHA256SUMS.txt")]
        self.assertEqual(len(checksum_calls), 1)
        self.assertTrue(checksum_calls[0]["url"].startswith("https://github.com/"))
        self.assertTrue((self.destination / "memoria").exists())

    def test_missing_official_checksum_blocks_install(self) -> None:
        response, _ = self._execute(sum_fail=True)
        self.assertNotEqual(response.returncode, 0)
        self.assertFalse((self.destination / "memoria").exists())

    def test_official_checksum_outage_blocks_mirrored_install(self) -> None:
        response, calls = self._execute(mirror=True, direct_fail=True, sum_fail=True)
        self.assertNotEqual(response.returncode, 0)
        self.assertFalse((self.destination / "memoria").exists())
        sums = [x for x in calls if x["url"].endswith("SHA256SUMS.txt")]
        self.assertEqual(len(sums), 1)
        self.assertTrue(sums[0]["url"].startswith("https://github.com/"))

    def test_mismatched_checksum_blocks_install(self) -> None:
        response, _ = self._execute(sum_corrupt=True)
        self.assertNotEqual(response.returncode, 0)
        self.assertFalse((self.destination / "memoria").exists())

    def test_malicious_mirror_binary_cannot_approve_its_own_checksum(self) -> None:
        response, calls = self._execute(mirror=True, direct_fail=True, malicious_mirror=True)
        self.assertNotEqual(response.returncode, 0)
        self.assertFalse((self.destination / "memoria").exists())
        checksums = [x for x in calls if x["url"].endswith("SHA256SUMS.txt")]
        self.assertEqual(len(checksums), 1)
        self.assertTrue(checksums[0]["url"].startswith("https://github.com/"))

    def test_dry_run_discloses_mirror_policy(self) -> None:
        response, calls = self._execute(dry_run=True)
        self.assertEqual(response.returncode, 0, response.stderr)
        self.assertIn("Mirror fallback: disabled", response.stdout)
        self.assertEqual(calls, [])
        response, _ = self._execute(dry_run=True, mirror=True)
        self.assertIn("https://mirror.example", response.stdout)

    def test_no_ten_second_total_download_cap(self) -> None:
        response, calls = self._execute()
        self.assertEqual(response.returncode, 0, response.stderr)
        download_calls = [x for x in calls if x["url"].endswith(".tar.gz")]
        self.assertTrue(download_calls)
        self.assertNotIn("--max-time", download_calls[0]["args"])
        self.assertIn("--connect-timeout", download_calls[0]["args"])


if __name__ == "__main__":
    unittest.main()
