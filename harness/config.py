"""Paths (contract §14.1) and every pin of appendix A, as data."""

import os
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

DUCKDB_VERSION = "v2.0.0"
CLICKHOUSE_VERSION = "26.8.34.13"
CLICKBENCH_COMMIT = "7276a62f955d5eac22f32ce1fd546b3953b05122"
DUCKDB_COMMIT = "d54cbbe3df17e8e5ce68312f483b4155836112aa"
PARQUET_TESTING_COMMIT = "eedfad3f086678d282a00d8ff3cf8932d5d29af5"

CLICKBENCH_RAW = f"https://raw.githubusercontent.com/ClickHouse/ClickBench/{CLICKBENCH_COMMIT}"
HITS_URL = "https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_{n}.parquet"
TPCH_QUERY_URL = (
    f"https://raw.githubusercontent.com/duckdb/duckdb/{DUCKDB_COMMIT}"
    "/extension/tpch/dbgen/queries/{name}.sql"
)
PARQUET_TESTING_RAW = (
    f"https://raw.githubusercontent.com/apache/parquet-testing/{PARQUET_TESTING_COMMIT}/data"
)


@dataclass(frozen=True)
class Paths:
    """Every location the harness reads or writes; nothing goes to `/tmp`."""

    repo: Path
    data: Path
    veta_bin: Path

    @property
    def bin_dir(self) -> Path:
        return self.data / "bin"

    @property
    def duckdb(self) -> Path:
        return self.bin_dir / "duckdb"

    @property
    def clickhouse(self) -> Path:
        return self.bin_dir / "clickhouse"

    @property
    def duckdb_ext(self) -> Path:
        return self.data / "duckdb-ext"

    @property
    def clickbench(self) -> Path:
        return self.data / "clickbench"

    @property
    def tpch(self) -> Path:
        return self.data / "tpch"

    @property
    def parquet_testing(self) -> Path:
        return self.data / "parquet-testing"

    @property
    def db(self) -> Path:
        return self.data / "db"

    @property
    def runs(self) -> Path:
        return self.data / "runs"

    @property
    def scratch(self) -> Path:
        return self.data / "scratch"


def paths(env=None) -> Paths:
    """Paths from `VETA_DATA` and `VETA_BIN`, defaulting inside the repository."""
    env = os.environ if env is None else env
    data = Path(env.get("VETA_DATA") or REPO / ".veta-data").resolve()
    veta_bin = Path(env.get("VETA_BIN") or REPO / "target" / "release" / "veta").resolve()
    return Paths(repo=REPO, data=data, veta_bin=veta_bin)


@dataclass(frozen=True)
class Pin:
    """One file of appendix A, relative to `VETA_DATA`.

    `url` is a direct download; with `member` the URL is an archive whose `archive_hash`
    (algorithm, hex) is checked before the member is extracted; with `install` the file
    comes from DuckDB's `INSTALL <install>`. `sha256` is always the final file's hash.
    """

    dest: str
    sha256: str
    url: str | None = None
    member: str | None = None
    archive_hash: tuple[str, str] | None = None
    install: str | None = None
    executable: bool = False


DUCKDB_PIN = Pin(
    dest="bin/duckdb",
    url=f"https://github.com/duckdb/duckdb/releases/download/{DUCKDB_VERSION}/duckdb-cli-linux-amd64.tar.gz",
    member="duckdb",
    archive_hash=("sha256", "c6507206baf7793c6013dbd82368469a45f20dc0e17a6f92f204602f652ed2bc"),
    sha256="c41f957db5645d545a9caef430994cf262d24fad9297fef70c691395aadb43fb",
    executable=True,
)

CLICKHOUSE_PIN = Pin(
    dest="bin/clickhouse",
    url=(
        f"https://github.com/ClickHouse/ClickHouse/releases/download/v{CLICKHOUSE_VERSION}-lts/"
        f"clickhouse-common-static-{CLICKHOUSE_VERSION}-amd64.tgz"
    ),
    member=f"clickhouse-common-static-{CLICKHOUSE_VERSION}/usr/bin/clickhouse",
    archive_hash=(
        "sha512",
        "e5c753b258679daa882747aaec10b351a85bcad1412af7957d4a4616dcf5b456"
        "63da219bd37fe0697fde2ae76dbed2ba5b414ce9a5c8044c6d1fa0142e03e4cc",
    ),
    sha256="3bc4d96279e811079a1a8c81620942e676f1f889ce39e585bb0cbec5f9c59ee9",
    executable=True,
)

TPCH_EXTENSION_PIN = Pin(
    dest=f"duckdb-ext/{DUCKDB_VERSION}/linux_amd64/tpch.duckdb_extension",
    install="tpch",
    sha256="2e6bdf1a41b907a59b886fdd048c885512410ed717e2474bd7a52a2bde32d42f",
)

# RESEARCH §4: hits_0..hits_9 of the partitioned ClickBench data set.
HITS_SHA256 = (
    "fa134fe101e68324e0de851146fda69624f5cbb707d387141d1c2a88a219a16d",
    "04f5af123af25d9a6e65584b8fd166fb9b651da11c777ed51379a79bcd52bee3",
    "089abaab27e608d1c201cdad0ef7b9e1cb8e84e662745d3e3a4372a7780d0ae0",
    "c28da906c92ee840400c52e29da1d50a9fd622911c8515b9ae30823bc72f2200",
    "42d8d0190a98a7b63a89461d13eec0d7cded44c34969d65c73bdb3fc3a5fc7ac",
    "0e00d3cdf0a0f6ccb5cbc78a2b241bf226725eb9a971bf82cda20d1d6e676e2f",
    "c96da6182478ec20035f14d67f51f4891fb6a3eb6913d5d465053aa2bf266226",
    "174d3dc0374ec5b369b7346a0096b6dc6a41502d6704212463962af8e9458b46",
    "0132c8c1be5a7d982fda1a165ab279cec7634ef1bb23131cf727da8278f0ec98",
    "a22bc7656b744537f259fe018f25bc52c4b06ecebd3ac96268fa2cb63f745f3f",
)
HITS_ROWS = 10_000_000

HITS_PINS = tuple(
    Pin(dest=f"clickbench/hits_{n}.parquet", url=HITS_URL.format(n=n), sha256=sha)
    for n, sha in enumerate(HITS_SHA256)
)

CLICKBENCH_SQL_PINS = (
    Pin(
        dest="clickbench/queries-clickhouse.sql",
        url=f"{CLICKBENCH_RAW}/clickhouse/queries.sql",
        sha256="a7d6673357348ee9680443216b6f26f30d1dce9f313b419d38502417b2c2a219",
    ),
    Pin(
        dest="clickbench/create-clickhouse.sql",
        url=f"{CLICKBENCH_RAW}/clickhouse/create.sql",
        sha256="c1082ee0d86467a7c66ce3059e37461333d7078b288935f7c93fd6d92102a3dd",
    ),
    Pin(
        dest="clickbench/queries-duckdb.sql",
        url=f"{CLICKBENCH_RAW}/duckdb/queries.sql",
        sha256="274ffe1c4f83baad2fc177bbb6773bbdab0db779faeb2970de8cc531292a5dc6",
    ),
    Pin(
        dest="clickbench/create-duckdb.sql",
        url=f"{CLICKBENCH_RAW}/duckdb/create.sql",
        sha256="8065807651b163ab7006b8e451d820e2f6ddc757a6c6bd033800fe1dbc3f81e7",
    ),
)

# Appendix A, table A.1.
TPCH_QUERY_SHA256 = {
    "q01": "8aefe77a1c97459bc69acbbcdaf3b8637d8ea22dfc3ac7be732d4446da83b69b",
    "q02": "a2219893f952dd2a2586ab297d909ec2d440faabb5e692f24880158aa6877344",
    "q03": "1f120d790d4914652cbf3fbc0fa28ed6573e4adee61ae7164a8c3d99b7c6b7fe",
    "q04": "2a2be89752fc917bc71e1a59168ead9d00d40c40ea0bbe646206e414b372f888",
    "q05": "495def7debb02ab1def31da07fbc88b4796813b754832ce15d54a35dcb104577",
    "q06": "3638535fb92a6026b3359a2ad9e6ed7da9ad9080152513e6fd3818d1a853c29f",
    "q07": "5e8077121a42ca8ebf5ff54e5842a614e839da945b0331e3b63d9ccad2361201",
    "q08": "948338f54b97a2c49aea15f8c4d609ecf244d00ad7754edcea60b4748504a1ce",
    "q09": "af90ccbe30ece396209e6627c10c0086ed54208100357922ee404de8bb867e56",
    "q10": "0b48b00cffa3776b32e67e9041191dea25095e2a952ee5251990b9c8585031ed",
    "q11": "f313c766fac507c1204fb6f2bacd35719a61a11c0f69601302555150dddeda78",
    "q12": "3814c9f4b7a476ee894c477d43cedb299603b1ed0f5315790d52839c8da355bf",
    "q13": "4a4a0b2c5646b2cad8f220a8f4b73ccb9f0acd45542353d63c18a69227bc20a3",
    "q14": "e7ec7960a4c49219fc76dc03495a1530aeb30c796413dd1dc9d501b86934f222",
    "q15": "1cb57383eeff60261a1b4b2a141c71d8672729d743a7dfa74d9a4bcaf69c44d4",
    "q16": "45085e7672cb1500e625e048156fc55369e7a9bc91fd3647c3f7735a870c3ef8",
    "q17": "dfc7f00ac8a500c2ac2735b05b41b462bbe4af9ce7cd6c37e42d1bb9df4a02e1",
    "q18": "8c5076061727d974f96708f0da930fc58843eac297a80c7ad658d64d152a19bb",
    "q19": "341f97d1f74deb30aca2a8b0ee849772a3b0e098a06cf12266c63573cb985478",
    "q20": "fa96ad5cb897bf31ec9b96a6b2a0d1f7132e71e2f481321e6db16628bc3e5fd5",
    "q21": "b29463f30a64e45a795047ee6e557e6c3c1f01fc612acc997873594048e74b59",
    "q22": "d8ed6332e75f9193391000ba071e17adcf735a2d99c5a4da0f7c13d695ccbd42",
}

TPCH_QUERY_PINS = tuple(
    Pin(dest=f"tpch/queries/{name}.sql", url=TPCH_QUERY_URL.format(name=name), sha256=sha)
    for name, sha in TPCH_QUERY_SHA256.items()
)

PARQUET_TESTING_PINS = (
    Pin(
        dest="parquet-testing/delta_byte_array.parquet",
        url=f"{PARQUET_TESTING_RAW}/delta_byte_array.parquet",
        sha256="a400b789aef5cde88551f25cdd9bba8f0ff0fe01c48ddc5303c26edf119ee279",
    ),
    Pin(
        dest="parquet-testing/delta_encoding_optional_column.parquet",
        url=f"{PARQUET_TESTING_RAW}/delta_encoding_optional_column.parquet",
        sha256="71f8f00b00ecc132a1cc5d534acca900d02d0fe4ba3525607b03b2bd06a56f1a",
    ),
)

# Fetch order: the DuckDB binary comes before the extension it installs.
ALL_PINS = (
    DUCKDB_PIN,
    TPCH_EXTENSION_PIN,
    CLICKHOUSE_PIN,
    *CLICKBENCH_SQL_PINS,
    *TPCH_QUERY_PINS,
    *PARQUET_TESTING_PINS,
    *HITS_PINS,
)
