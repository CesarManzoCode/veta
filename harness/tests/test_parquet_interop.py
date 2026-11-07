import unittest
from pathlib import Path

from harness import parquet_interop as pi
from harness.engines import EngineRun
from harness.parquet_interop import CheckError, Chunk, Export
from harness.report import FAIL, PASS

FILE = Path("/data/scratch/x/f.parquet")


def run(engine, stdout, code=0):
    return EngineRun(engine, [], code, stdout, b"", None, stdout)


class ParquetMetaTest(unittest.TestCase):
    def test_parse_parquet_meta(self):
        text = (
            "0\tl_orderkey\tINT64\tPLAIN,RLE_DICTIONARY\tSNAPPY\t1\t6000000\t0\n"
            "0\tl_shipdate\tINT32\tDELTA_BINARY_PACKED\tUNCOMPRESSED\t1992-01-02\t1998-12-01\t0\n"
            "1\tl_comment\tBYTE_ARRAY\tPLAIN\tSNAPPY\ta\\tb\tz\t\\N\n"
            "1\tl_price\tINT64\tPLAIN\tSNAPPY\t901.00\t104949.50\t3\n"
        )
        chunks = pi.parse_parquet_meta(text)
        self.assertEqual(chunks[0], Chunk(0, "l_orderkey", "INT64", ("PLAIN", "RLE_DICTIONARY"), "SNAPPY", 1, 6000000, 0))
        self.assertEqual(chunks[1].min, "1992-01-02")
        self.assertEqual(chunks[1].encodings, ("DELTA_BINARY_PACKED",))
        self.assertEqual((chunks[2].min, chunks[2].null_count), ("a\tb", None))
        self.assertEqual((chunks[3].min, chunks[3].max, chunks[3].null_count), (901.0, 104949.5, 3))
        self.assertEqual(pi.columns_of(chunks), ["l_orderkey", "l_shipdate"])

    def test_parse_parquet_meta_rejects_bad_rows(self):
        for text in ("0\ta\tINT64\tPLAIN\tSNAPPY\t1\t2\n", "x\ta\tINT64\tPLAIN\tSNAPPY\t1\t2\t0\n",
                     "0\ta\tINT64\tPLAIN\tSNAPPY\t1\t2\tabc\n"):
            with self.assertRaises(ValueError):
                pi.parse_parquet_meta(text)
        self.assertEqual(pi.parse_parquet_meta(""), [])

    def test_split_encodings(self):
        self.assertEqual(pi.split_encodings("PLAIN,RLE"), ("PLAIN", "RLE"))
        self.assertEqual(pi.split_encodings("PLAIN_DICTIONARY, PLAIN"), ("PLAIN_DICTIONARY", "PLAIN"))
        self.assertEqual(pi.split_encodings("[RLE_DICTIONARY]"), ("RLE_DICTIONARY",))
        self.assertEqual(pi.split_encodings(""), ())

    def test_parse_duckdb_metadata_and_row_groups(self):
        text = (
            "0,a,INT64,\"PLAIN, RLE_DICTIONARY\",SNAPPY,1,9,0,100000\n"
            "0,b,BYTE_ARRAY,PLAIN,SNAPPY,\"x,y\",z,2,100000\n"
            "1,a,INT64,PLAIN,SNAPPY,10,12,0,3\n"
            "1,b,BYTE_ARRAY,PLAIN,SNAPPY,\\N,\\N,3,3\n"
        )
        chunks = pi.parse_duckdb_metadata(text)
        self.assertEqual(chunks[0].encodings, ("PLAIN", "RLE_DICTIONARY"))
        self.assertEqual((chunks[1].min, chunks[1].null_count, chunks[1].rows), ("x,y", 2, 100000))
        self.assertEqual((chunks[3].min, chunks[3].max), (None, None))
        self.assertEqual(pi.row_group_rows(chunks), [100000, 3])
        self.assertEqual(pi.columns_of(chunks), ["a", "b"])
        with self.assertRaises(ValueError):
            pi.row_group_rows([Chunk(1, "a", "INT64", (), "", None, None, 0, 5)])

    def test_coverage_counts_rle_through_nulls(self):
        chunks = [Chunk(0, "a", "INT64", ("PLAIN_DICTIONARY", "PLAIN"), "SNAPPY", 1, 2, 0),
                  Chunk(0, "b", "DOUBLE", ("BYTE_STREAM_SPLIT",), "UNCOMPRESSED", 1, 2, 0)]
        self.assertEqual(pi.covered(chunks), ({"PLAIN_DICTIONARY", "PLAIN", "BYTE_STREAM_SPLIT"},
                                              {"SNAPPY", "UNCOMPRESSED"}))
        chunks.append(Chunk(1, "b", "DOUBLE", ("PLAIN",), "SNAPPY", 1, 2, 4))
        self.assertIn("RLE", pi.covered(chunks)[0])

    def test_read_duckdb_verdict(self):
        items = [{"verdict": "equal"}, {"verdict": "equal"}]
        everything = set(pi.REQUIRED_ENCODINGS)
        compressions = set(pi.REQUIRED_COMPRESSIONS)
        status, summary = pi.read_duckdb_verdict(items, everything, compressions)
        self.assertEqual((status, summary), (PASS, "2/2 equal, encodings 8/8"))
        status, summary = pi.read_duckdb_verdict(items, everything - {"DELTA_BYTE_ARRAY"}, compressions)
        self.assertEqual(status, FAIL)
        self.assertIn("missing DELTA_BYTE_ARRAY", summary)
        self.assertEqual(pi.read_duckdb_verdict(items, everything, {"SNAPPY"})[0], FAIL)
        self.assertEqual(pi.read_duckdb_verdict([{"verdict": "different"}], everything, compressions)[0], FAIL)
        self.assertEqual(pi.read_duckdb_verdict([], everything, compressions)[0], FAIL)


class ReadSqlTest(unittest.TestCase):
    def test_copy_sql(self):
        options = pi.copy_options((("PARQUET_VERSION", "v2"), ("DICTIONARY_SIZE_LIMIT", "1")), "snappy")
        sql = pi.duckdb_copy_sql("SELECT * FROM t", Path("/d/o'k.parquet"), options)
        self.assertEqual(sql, "COPY (SELECT * FROM t) TO '/d/o''k.parquet' (FORMAT parquet, COMPRESSION snappy, "
                              "ROW_GROUP_SIZE 10240, PARQUET_VERSION v2, DICTIONARY_SIZE_LIMIT 1);")

    def test_configurations_cover_contract(self):
        self.assertEqual(len(pi.READ_CONFIGS) * len(pi.READ_COMPRESSIONS), 8)
        self.assertIn("CAST(l_extendedprice AS DOUBLE)", pi.LINEITEM_SAMPLE_SQL)
        self.assertIn("NULL", pi.LINEITEM_SAMPLE_SQL)

    def test_sources(self):
        self.assertEqual(pi.duckdb_source(FILE), f"read_parquet('{FILE}', binary_as_string=true)")
        self.assertEqual(pi.duckdb_source(FILE, row_numbers=True),
                         f"read_parquet('{FILE}', binary_as_string=true, file_row_number=true)")
        self.assertEqual(pi.veta_source(FILE), f"read_parquet('{FILE}')")
        self.assertEqual(pi.clickhouse_source(FILE), f"file('{FILE}', Parquet)")
        self.assertEqual(pi.ident('a"b'), '"a""b"')

    def test_hits_sums(self):
        chunks = [Chunk(0, "WatchID", "INT64", (), "", None, None, 0), Chunk(0, "URL", "BYTE_ARRAY", (), "", None, None, 0),
                  Chunk(0, "Flag", "BOOLEAN", (), "", None, None, 0), Chunk(1, "WatchID", "INT64", (), "", None, None, 0)]
        columns = pi.hits_columns(chunks)
        self.assertEqual(columns, [("WatchID", "numeric"), ("URL", "string")])
        self.assertEqual(pi.read_hits_sql(columns, "src", "veta"),
                         'SELECT count(*), sum("WatchID"), sum(length("URL")) FROM src')
        self.assertEqual(pi.read_hits_sql(columns, "src", "duckdb"),
                         'SELECT count(*), sum("WatchID"), sum(strlen("URL")) FROM src')
        labels = pi.aggregate_labels(columns)
        self.assertEqual(labels, ["count(*)", "sum(WatchID)", "sum(length(URL))"])
        self.assertEqual(pi.cell_differences(labels, (3, 10, 7), (3, 10, 7)), [])
        self.assertEqual(pi.cell_differences(labels, (3, 10, 7), (3, 11, 7)), ["sum(WatchID): veta 10 != 11"])
        self.assertEqual(len(pi.cell_differences(labels, (3, 10), (3, 10, 7))), 1)

    def test_single_row(self):
        self.assertEqual(pi.single_row(run("veta", b"1\t2\n"), "x"), (1, 2))
        self.assertEqual(pi.single_row(run("duckdb", b"1,2\n"), "x"), (1, 2))
        with self.assertRaises(CheckError):
            pi.single_row(run("veta", b"1\n2\n"), "x")
        with self.assertRaises(CheckError):
            pi.single_row(run("veta", b"", code=1), "x")


class ExceptAllTest(unittest.TestCase):
    def test_exports(self):
        exports = pi.write_exports()
        self.assertEqual(len(exports), 2 + 8 * 4 * 2)
        self.assertEqual(exports[0], Export("hits", "clickbench", "dict", "snappy"))
        self.assertEqual(exports[1].name, "hits-delta-none")
        self.assertIn(Export("lineitem", "tpch", "delta-byte-array", "none"), exports)
        self.assertEqual(len({e.name for e in exports}), len(exports))

    def test_export_argv(self):
        argv = pi.export_argv(Path("/db"), "SELECT * FROM t", Path("/o.parquet"), "dict", "none")
        self.assertEqual(argv, ["export", "--db", Path("/db"), "--sql", "SELECT * FROM t", "--out",
                                Path("/o.parquet"), "--encoding", "dict", "--compression", "none"])
        argv = pi.export_argv(Path("/db"), "q", Path("/o"), row_group_rows=100_000)
        self.assertEqual(argv[-2:], ["--row-group-rows", 100_000])

    def test_match_columns(self):
        oracle = [("EventTime", "TIMESTAMP"), ("URL", "VARCHAR")]
        self.assertEqual(pi.match_columns(oracle, ["url", "eventtime"]),
                         [("eventtime", "EventTime", "TIMESTAMP"), ("url", "URL", "VARCHAR")])
        with self.assertRaises(CheckError):
            pi.match_columns(oracle, ["url"])
        with self.assertRaises(CheckError):
            pi.match_columns(oracle, ["url", "eventtime", "extra"])
        with self.assertRaises(CheckError):
            pi.match_columns(oracle, ["url", "URL", "eventtime"])

    def test_except_all_sql(self):
        columns = [("eventtime", "EventTime", "TIMESTAMP"), ("x", "x", "DECIMAL(15,2)")]
        left = pi.file_select(columns, "read_parquet('f')")
        self.assertEqual(left, 'SELECT CAST("eventtime" AS TIMESTAMP) AS "EventTime", '
                               'CAST("x" AS DECIMAL(15,2)) AS "x" FROM read_parquet(\'f\')')
        right = pi.table_select(columns, "hits")
        self.assertEqual(right, 'SELECT "EventTime", "x" FROM hits')
        self.assertEqual(pi.except_all_sql("L", "R"),
                         "SELECT (SELECT count(*) FROM (L EXCEPT ALL R) AS only_file), "
                         "(SELECT count(*) FROM (R EXCEPT ALL L) AS only_table)")

    def test_except_verdict(self):
        self.assertEqual(pi.except_verdict(0, 0), ("equal", ""))
        self.assertEqual(pi.except_verdict(2, 0)[0], "different")
        self.assertEqual(pi.except_counts((0, 3)), (0, 3))
        with self.assertRaises(CheckError):
            pi.except_counts((0,))
        with self.assertRaises(CheckError):
            pi.except_counts((0, None))

    def test_schema_queries(self):
        self.assertEqual(pi.duckdb_columns_sql("hits"),
                         "SELECT column_name, data_type FROM information_schema.columns "
                         "WHERE table_name = 'hits' ORDER BY ordinal_position")
        self.assertIn("database = 'tpch_sf1' AND table = 'orders'", pi.clickhouse_columns_sql("tpch_sf1", "orders"))
        # DuckDB's -csv quotes a type that contains a comma.
        duck = pi.parse_columns(run("duckdb", b'a,"DECIMAL(15,2)"\nb,VARCHAR\n'), "x")
        self.assertEqual(duck, [("a", "DECIMAL(15,2)"), ("b", "VARCHAR")])
        house = pi.parse_columns(run("clickhouse", b"a\tNullable(Int64)\t\t\t\t\t\nb\tString\t\t\t\t\t\n"), "x")
        self.assertEqual(house, [("a", "Nullable(Int64)"), ("b", "String")])
        with self.assertRaises(CheckError):
            pi.parse_columns(run("duckdb", b""), "x")


class RowGroupStatsTest(unittest.TestCase):
    def test_row_group_case(self):
        self.assertEqual(pi.row_group_case([5]), "0")
        self.assertEqual(pi.row_group_case([100, 100, 7]),
                         "CASE WHEN file_row_number < 100 THEN 0 WHEN file_row_number < 200 THEN 1 ELSE 2 END")

    def test_row_group_stats_sql(self):
        sql = pi.row_group_stats_sql(FILE, ["a", "b"], [10, 3])
        self.assertEqual(sql, (
            'SELECT veta_row_group, min("a"), max("a"), count(*) - count("a"), '
            'min("b"), max("b"), count(*) - count("b") FROM (SELECT *, '
            "CASE WHEN file_row_number < 10 THEN 0 ELSE 1 END AS veta_row_group "
            f"FROM read_parquet('{FILE}', binary_as_string=true, file_row_number=true)) AS t "
            "GROUP BY veta_row_group ORDER BY veta_row_group;"
        ))

    def test_parse_row_group_stats(self):
        stats = pi.parse_row_group_stats("0,1,9,0,a,z,2\n1,10,12,0,\\N,\\N,3\n", ["a", "b"])
        self.assertEqual(stats[(0, "a")], (1, 9, 0))
        self.assertEqual(stats[(0, "b")], ("a", "z", 2))
        self.assertEqual(stats[(1, "b")], (None, None, 3))
        with self.assertRaises(ValueError):
            pi.parse_row_group_stats("0,1,9\n", ["a", "b"])

    def test_stats_differences(self):
        veta = {(0, "a"): (1, 9, 0), (0, "p"): (901.0, 1000.5, 0)}
        footer = {(0, "a"): (1, 9, 0), (0, "p"): (901.0, 1000.5, 0)}
        rows = {(0, "a"): (1, 9, 0), (0, "p"): (901.0, 1000.5, 0)}
        self.assertEqual(pi.stats_differences(veta, footer, rows), [])
        rows[(0, "a")] = (1, 8, 0)
        diffs = pi.stats_differences(veta, footer, rows)
        self.assertEqual(diffs, ["rg 0 a max: veta 9, parquet_metadata 9, rows 8"])
        del footer[(0, "p")]
        self.assertIn("rg 0 p: missing in parquet_metadata", pi.stats_differences(veta, footer, rows))


class RowGroupSkipTest(unittest.TestCase):
    def test_queries(self):
        queries = pi.skip_queries(FILE)
        self.assertEqual(len(queries), 5)
        self.assertEqual(queries[0], ("s1", f"SELECT count(*) FROM read_parquet('{FILE}') "
                                            "WHERE l_shipdate = DATE '1995-06-17'"))
        self.assertTrue(all("l_shipdate" in sql for _, sql in queries))

    def test_row_groups_and_verdict(self):
        stderr = b'VETA_STATS {"row_groups_total":60,"row_groups_read":2,"granules_read":0}\n'
        r = EngineRun("veta", [], 0, b"", stderr, 0.1, b"")
        self.assertEqual(pi.row_groups(r), (2, 60))
        self.assertEqual(pi.row_groups(None), (None, None))
        self.assertEqual(pi.skip_verdict("equal", "", 2, 60), ("equal", ""))
        self.assertEqual(pi.skip_verdict("equal", "", 60, 60)[0], "different")
        self.assertEqual(pi.skip_verdict("equal", "", None, 60)[0], "error")
        self.assertEqual(pi.skip_verdict("different", "rows", 2, 60), ("different", "rows"))


if __name__ == "__main__":
    unittest.main()
