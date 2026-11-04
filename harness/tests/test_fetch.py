import hashlib
import io
import tarfile
import unittest
from pathlib import Path

from harness import fetch
from harness.config import Paths, Pin
from harness.tests.support import scratch_dir


def sha(data: bytes, algorithm: str = "sha256") -> str:
    return hashlib.new(algorithm, data).hexdigest()


def quiet(_message):
    pass


class HashTest(unittest.TestCase):
    def test_verifies_against_fake_files(self):
        with scratch_dir() as d:
            f = Path(d) / "hits_0.parquet"
            f.write_bytes(b"PAR1 fake")
            self.assertEqual(fetch.file_hash(f), sha(b"PAR1 fake"))
            self.assertTrue(fetch.verifies(f, sha(b"PAR1 fake")))
            self.assertTrue(fetch.verifies(f, sha(b"PAR1 fake").upper()))
            self.assertFalse(fetch.verifies(f, sha(b"PAR1 other")))
            self.assertFalse(fetch.verifies(Path(d) / "absent", sha(b"")))
            self.assertTrue(fetch.verifies(f, sha(b"PAR1 fake", "sha512"), "sha512"))


class DownloadTest(unittest.TestCase):
    """`file://` URLs exercise the download path without network."""

    def test_download_renames_only_when_verified(self):
        with scratch_dir() as d:
            src = Path(d) / "src.sql"
            src.write_bytes(b"SELECT 1;\n")
            dest = Path(d) / "out" / "q01.sql"
            fetch.download(src.as_uri(), dest, sha(b"SELECT 1;\n"), log=quiet)
            self.assertEqual(dest.read_bytes(), b"SELECT 1;\n")
            self.assertFalse(dest.with_name("q01.sql.part").exists())

            bad = Path(d) / "out" / "q02.sql"
            with self.assertRaises(fetch.FetchError):
                fetch.download(src.as_uri(), bad, sha(b"other"), log=quiet)
            self.assertFalse(bad.exists())
            self.assertFalse(bad.with_name("q02.sql.part").exists())

    def test_fetch_pin_is_idempotent(self):
        with scratch_dir() as d:
            root = Path(d)
            src = root / "upstream.parquet"
            src.write_bytes(b"data")
            p = Paths(repo=root, data=root / "data", veta_bin=root / "veta")
            pin = Pin(dest="parquet-testing/x.parquet", url=src.as_uri(), sha256=sha(b"data"))
            self.assertEqual(fetch.fetch_pin(pin, p, quiet), "fetched")
            src.write_bytes(b"changed upstream")
            self.assertEqual(fetch.fetch_pin(pin, p, quiet), "verified")
            (p.data / pin.dest).write_bytes(b"corrupt")
            with self.assertRaises(fetch.FetchError):
                fetch.fetch_pin(pin, p, quiet)


class ArchiveTest(unittest.TestCase):
    @staticmethod
    def tarball(path: Path, members: dict[str, bytes]) -> bytes:
        with tarfile.open(path, "w:gz") as tar:
            for name, data in members.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        return path.read_bytes()

    def test_extracts_only_the_member(self):
        with scratch_dir() as d:
            root = Path(d)
            archive = root / "pkg.tgz"
            self.tarball(archive, {"pkg/usr/bin/clickhouse": b"ELF fake", "pkg/other": b"x"})
            dest = root / "bin" / "clickhouse"
            fetch.extract_member(archive, "pkg/usr/bin/clickhouse", dest, sha(b"ELF fake"), executable=True)
            self.assertEqual(dest.read_bytes(), b"ELF fake")
            self.assertTrue(dest.stat().st_mode & 0o100)
            self.assertEqual(sorted(x.name for x in (root / "bin").iterdir()), ["clickhouse"])
            self.assertFalse((root / "pkg").exists())

    def test_member_errors(self):
        with scratch_dir() as d:
            root = Path(d)
            archive = root / "pkg.tar.gz"
            self.tarball(archive, {"duckdb": b"ELF"})
            with self.assertRaises(fetch.FetchError):
                fetch.extract_member(archive, "missing", root / "bin" / "duckdb", sha(b"ELF"), True)
            with self.assertRaises(fetch.FetchError):
                fetch.extract_member(archive, "duckdb", root / "bin" / "duckdb", sha(b"other"), True)
            self.assertFalse((root / "bin" / "duckdb").exists())

    def test_fetch_pin_through_archive(self):
        with scratch_dir() as d:
            root = Path(d)
            upstream = root / "duckdb-cli.tar.gz"
            data = self.tarball(upstream, {"duckdb": b"ELF duckdb"})
            p = Paths(repo=root, data=root / "data", veta_bin=root / "veta")
            pin = Pin(dest="bin/duckdb", url=upstream.as_uri(), member="duckdb",
                      archive_hash=("sha512", sha(data, "sha512")), sha256=sha(b"ELF duckdb"), executable=True)
            self.assertEqual(fetch.fetch_pin(pin, p, quiet), "fetched")
            self.assertEqual((p.data / "bin" / "duckdb").read_bytes(), b"ELF duckdb")
            self.assertFalse((p.scratch / "fetch" / upstream.name).exists())
            self.assertEqual(fetch.fetch_pin(pin, p, quiet), "verified")

    def test_wrong_archive_hash_is_rejected(self):
        with scratch_dir() as d:
            root = Path(d)
            upstream = root / "ch.tgz"
            self.tarball(upstream, {"clickhouse": b"ELF"})
            p = Paths(repo=root, data=root / "data", veta_bin=root / "veta")
            pin = Pin(dest="bin/clickhouse", url=upstream.as_uri(), member="clickhouse",
                      archive_hash=("sha512", sha(b"not it", "sha512")), sha256=sha(b"ELF"))
            with self.assertRaises(fetch.FetchError):
                fetch.fetch_pin(pin, p, quiet)
            self.assertFalse((p.data / "bin" / "clickhouse").exists())


if __name__ == "__main__":
    unittest.main()
