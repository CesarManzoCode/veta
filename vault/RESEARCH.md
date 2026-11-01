# Investigación — veta

Fecha: 2026-11-01. Etiquetas: **[medido]** = ejecutado en esta máquina en K0; **[doc]** = documentación oficial o fuente primaria con su revisión fijada; **[hipótesis]** = no verificado.

## 1. Máquina de referencia [medido]

| Dato | Valor |
|---|---|
| CPU | AMD Ryzen 5 5600G, x86_64, 6 núcleos / 12 hilos, AVX2, BMI2, SSE4.2, L3 16 MiB |
| RAM | 14 GiB visibles para el sistema (`free -g`), swap 3 GiB |
| Disco | `/home` con 368 GB libres; `/tmp` es tmpfs de 7.5 GB, así que los datos no pueden ir ahí |
| SO | Arch Linux, kernel 7.2.11, sin root |
| Rust | `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1`, `clippy 0.1.98`, `rustfmt 1.9.0` |
| Python | `Python 3.14.8` (el runner de CI `ubuntu-24.04` trae 3.12.3) |
| Otras | `lz4 v1.10.0`, `zstd v1.5.7`, GNU `time 1.10` en `/usr/bin/time`, `cloc 2.10`, `curl 8.23.0`, `git 2.56.0`, `gh 2.102.1`. No hay `strace` ni `perf` |

`std::fs::File::lock`, `lock_shared` y `try_lock` funcionan con rustc 1.98.1 (programa de prueba compilado y ejecutado) [medido].

## 2. Fuentes primarias con revisión fijada [doc]

| Fuente | Revisión fijada | Uso |
|---|---|---|
| ClickBench, https://github.com/ClickHouse/ClickBench | commit `7276a62f955d5eac22f32ce1fd546b3953b05122` (2026-11-01) | Consultas, esquemas y método de carga. Licencia CC BY-NC-SA 4.0: no se copia al repo, se descarga |
| Datos de ClickBench, `https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_{N}.parquet` | sha256 de cada archivo en §5 | Subconjunto `hits_0..hits_9` |
| DuckDB, https://github.com/duckdb/duckdb | tag `v2.0.0`, commit `d54cbbe3df17e8e5ce68312f483b4155836112aa` | Oráculo; consultas TPC-H en `extension/tpch/dbgen/queries/qNN.sql` (MIT) |
| Calendario de versiones de DuckDB, https://duckdb.org/release_calendar | consultado 2026-11-01 | 2.0.0 es LTS |
| Extensión tpch de DuckDB, https://duckdb.org/docs/stable/core_extensions/tpch | consultado 2026-11-01 | `dbgen`, `tpch_queries()`, `tpch_answers()` |
| ClickHouse, https://github.com/ClickHouse/ClickHouse | tag `v26.8.34.13-lts` (2026-10-22) | Oráculo y referencia de tiempo |
| Formato Parquet, https://github.com/apache/parquet-format | tag `apache-parquet-format-2.14.0`, commit `04d56f291ff963e98bc37ab8100e2fc133ff583c` | `src/main/thrift/parquet.thrift`, `Encodings.md`, `Compression.md`, `PageIndex.md` |
| Corpus de prueba Parquet, https://github.com/apache/parquet-testing | commit `eedfad3f086678d282a00d8ff3cf8932d5d29af5` | `data/delta_byte_array.parquet`, `data/delta_encoding_optional_column.parquet` |
| Protocolo Thrift compacto, https://github.com/apache/thrift | tag `v0.25.0`, commit `27e8a425ffb498e190df3a12e239326bf5ba9ed6`, `doc/specs/thrift-compact-protocol.md` | Codificación de metadatos Parquet |
| Formato Snappy, https://github.com/google/snappy | tag `1.3.1`, commit `9c28114a38866f6deeaa826db918293bc28ae410`, `format_description.txt` | Compresión SNAPPY de Parquet |
| Formato de bloque LZ4, https://github.com/lz4/lz4 | tag `v1.10.0`, commit `ebb370ca83af193212df4dcbadcc5d87bc0de2f0`, `doc/lz4_Block_format.md` | Solo referencia de comparación; el compresor propio no lo implementa |
| Leis et al., *Morsel-Driven Parallelism*, SIGMOD 2014, https://db.in.tum.de/~leis/papers/morsels.pdf | publicación final | Modelo de ejecución |
| Leis et al., *How Good Are Query Optimizers, Really?*, PVLDB 9(3) 2015, https://www.vldb.org/pvldb/vol9/p204-leis.pdf | publicación final | Estimación de cardinalidad y error q |
| Moerkotte y Neumann, *Analysis of Two Existing and One New Dynamic Programming Algorithm for the Generation of Optimal Bushy Join Trees without Cross Products*, VLDB 2006, https://www.vldb.org/conf/2006/p930-moerkotte.pdf | publicación final | Enumeración DPccp |
| Rigger y Su, *Finding Bugs in Database Systems via Query Partitioning* (TLP), OOPSLA 2020, doi:10.1145/3428279 | publicación final | Oráculo TLP |
| Heule et al., *HyperLogLog in Practice*, EDBT 2013, doi:10.1145/2452376.2452456 | publicación final | Estimación de valores distintos |
| Vacante, https://jobs.ashbyhq.com/posthog/43dbf072-0fc1-48c9-8c1e-7416db7d4a14 y https://api.ashbyhq.com/posting-api/job-board/posthog | consultada 2026-11-01 por la investigación adjunta | Propósito |

## 3. Herramientas externas verificadas en esta máquina [medido]

### DuckDB 2.0.0

- Descarga: `https://github.com/duckdb/duckdb/releases/download/v2.0.0/duckdb-cli-linux-amd64.tar.gz`, sha256 del tarball `c6507206baf7793c6013dbd82368469a45f20dc0e17a6f92f204602f652ed2bc` (coincide con el `digest` de la API de GitHub). Binario extraído `duckdb`, 77 526 584 bytes, sha256 `c41f957db5645d545a9caef430994cf262d24fad9297fef70c691395aadb43fb`. `duckdb --version` → `v2.0.0 (Cyanoptera) d54cbbe3df`.
- **Modo agente**: con la variable `AI_AGENT` definida y stdout que no es terminal, la CLI cambia a tablas markdown con hash y errores en JSON. Lo apagan `-no-agent`, `DUCKDB_AGENT_MODE=0` o cualquier modo de salida explícito (`-csv`, `-json`). `-no-agent` no existe en v1.5.6.
- Flags usados: `-no-agent`, `-csv`, `-noheader`, `-nullvalue TEXT`, `-c COMMAND` (repetible), `-cmd COMMAND`, `-json`. `-c ".timer on"` imprime en stdout `Run Time (s): real 0.000 user … sys …` (resolución de 1 ms).
- Ajustes: `extension_directories` (lista; `extension_directory` está deprecado y avisa en stderr), `memory_limit`, `threads`, `binary_as_string`, `autoinstall_known_extensions`.
- **tpch no viene incluida**: `duckdb_extensions()` la reporta `NOT_INSTALLED`. `SET extension_directories=['<dir>']; INSTALL tpch;` la descarga (necesita red una sola vez) de `http://extensions.duckdb.org/v2.0.0/linux_amd64/tpch.duckdb_extension.gz`; el archivo instalado pesa 29 427 998 bytes, sha256 `2e6bdf1a41b907a59b886fdd048c885512410ed717e2474bd7a52a2bde32d42f`. Después `LOAD tpch; CALL dbgen(sf=0.01)` funciona sin red (probado con `unshare -rn`).
- `tpch_answers()` tiene 66 filas (22 consultas × escalas 0.01, 0.1 y 1). Cada `answer` es texto separado por `|` con línea de cabecera. `tpch_queries()` da los 22 textos, idénticos byte a byte (tras `strip`) a `extension/tpch/dbgen/queries/qNN.sql` del commit `d54cbbe3`.
- Escritura Parquet (`COPY … TO … (FORMAT parquet, …)`): por omisión SNAPPY, PLAIN y PLAIN_DICTIONARY; con `PARQUET_VERSION v2`: RLE_DICTIONARY, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY y BYTE_STREAM_SPLIT (DOUBLE); `DICTIONARY_SIZE_LIMIT 1` quita el diccionario; `COMPRESSION uncompressed` funciona. Booleanos: PLAIN en v1 y v2. **No escribe DELTA_BYTE_ARRAY** en ninguna combinación probada. Columnas con NULL: niveles de definición RLE.
- Lectura: lee DELTA_BYTE_ARRAY (`parquet-testing/data/delta_byte_array.parquet`, 1000 filas). `parquet_metadata()` expone `stats_min_value`, `stats_max_value`, `stats_null_count`, `encodings`, `compression`, `row_group_num_rows`; `read_parquet(…, file_row_number=true)` da la posición de fila. `EXCEPT ALL` funciona.
- Tipos que exporta DuckDB para TPC-H: DECIMAL(15,2) como INT64 con anotación DECIMAL, DATE como INT32, VARCHAR como BYTE_ARRAY UTF8, BIGINT/INTEGER como INT64/INT32.
- SF10: `CALL dbgen(sf=10)` en memoria tarda 32.1 s y llega a **12.85 GB de RSS**; en una base en disco con `SET memory_limit='4GB'` tarda 21.5 s con 4.19 GB de RSS y deja un archivo de 2.83 GB. `lineitem` SF10 = 59 986 052 filas; su Parquet pesa 2 206 307 029 bytes; los 8 Parquet de SF10 suman 3.2 GB.

### ClickHouse 26.8.34.13-lts

- Descarga: `https://github.com/ClickHouse/ClickHouse/releases/download/v26.8.34.13-lts/clickhouse-common-static-26.8.34.13-amd64.tgz` (237 270 667 bytes), sha512 `e5c753b258679daa882747aaec10b351a85bcad1412af7957d4a4616dcf5b45663da219bd37fe0697fde2ae76dbed2ba5b414ce9a5c8044c6d1fa0142e03e4cc` (coincide con el `.sha512` publicado), sha256 `d64b69ce0a13a1e89960b5ffcc6f07ea471b314ae21348e457dd232c8fa5506e`. Binario `clickhouse-common-static-26.8.34.13/usr/bin/clickhouse`, 799 412 608 bytes, sha256 `3bc4d96279e811079a1a8c81620942e676f1f889ce39e585bb0cbec5f9c59ee9`. `clickhouse --version` → `ClickHouse local version 26.8.34.13 (official build)`. No necesita root.
- `clickhouse local` con `--path DIR` guarda tablas MergeTree persistentes; flags usados: `-q`, `--queries-file`, `--format TSV`, `--time` (segundos en stderr), `--session_timezone UTC`, `--join_use_nulls 1`, `--database`.
- Ajustes por omisión relevantes: `join_use_nulls=0`, `enable_positional_arguments=1`, `allow_experimental_correlated_subqueries=1`, `max_threads=auto(12)`, `output_format_parquet_compression_method=zstd`.
- Ejecuta las 22 consultas TPC-H de DuckDB sin cambios sobre tablas MergeTree cargadas desde Parquet.
- Escritura Parquet: solo PLAIN, RLE_DICTIONARY y RLE (niveles); compresión configurable (`snappy`, `none`, …). Lee los Parquet v2 de DuckDB y DELTA_BYTE_ARRAY. `EXCEPT ALL` funciona.

### lz4 1.10.0 (sistema)

`lz4 -b1 -i3 -q ARCHIVO` imprime una línea `-1  <comprimido> (<ratio>) <MB/s compresión> MB/s <MB/s descompresión> MB/s  <archivo>`.

## 4. Datos de ClickBench [medido]

- `hits.parquet` único: 14 779 976 446 bytes (13.76 GiB). Los 100 particionados suman 13.73 GiB; cada uno pesa entre 32.1 y 346.1 MiB.
- `hits_N.parquet`: 1 000 000 filas, 105 columnas, 2 grupos de filas (máx. 549 440), escritos por `parquet-cpp version 1.5.1-SNAPSHOT`, formato 1, compresión SNAPPY, codificaciones PLAIN_DICTIONARY, PLAIN y RLE. Todas las columnas son OPTIONAL sin ningún NULL (`stats_null_count` suma 0). Cadenas BYTE_ARRAY sin anotación; `EventTime`, `ClientEventTime`, `LocalEventTime` INT64 (segundos Unix); `EventDate` INT32 UINT_16 (días); enteros INT16/INT32/INT64.
- Subconjunto elegido `hits_0..hits_9` (10 000 000 filas, 1 486 924 682 bytes). sha256:

| Archivo | Bytes | sha256 |
|---|---|---|
| hits_0.parquet | 122446530 | fa134fe101e68324e0de851146fda69624f5cbb707d387141d1c2a88a219a16d |
| hits_1.parquet | 174965044 | 04f5af123af25d9a6e65584b8fd166fb9b651da11c777ed51379a79bcd52bee3 |
| hits_2.parquet | 230595491 | 089abaab27e608d1c201cdad0ef7b9e1cb8e84e662745d3e3a4372a7780d0ae0 |
| hits_3.parquet | 192507052 | c28da906c92ee840400c52e29da1d50a9fd622911c8515b9ae30823bc72f2200 |
| hits_4.parquet | 140929275 | 42d8d0190a98a7b63a89461d13eec0d7cded44c34969d65c73bdb3fc3a5fc7ac |
| hits_5.parquet | 122286439 | 0e00d3cdf0a0f6ccb5cbc78a2b241bf226725eb9a971bf82cda20d1d6e676e2f |
| hits_6.parquet | 122927935 | c96da6182478ec20035f14d67f51f4891fb6a3eb6913d5d465053aa2bf266226 |
| hits_7.parquet | 123902333 | 174d3dc0374ec5b369b7346a0096b6dc6a41502d6704212463962af8e9458b46 |
| hits_8.parquet | 122735269 | 0132c8c1be5a7d982fda1a165ab279cec7634ef1bb23131cf727da8278f0ec98 |
| hits_9.parquet | 133629314 | a22bc7656b744537f259fe018f25bc52c4b06ecebd3ac96268fa2cb63f745f3f |

- Archivos de ClickBench en el commit fijado (sha256): `clickhouse/queries.sql` `a7d6673357348ee9680443216b6f26f30d1dce9f313b419d38502417b2c2a219`; `clickhouse/create.sql` `c1082ee0d86467a7c66ce3059e37461333d7078b288935f7c93fd6d92102a3dd`; `duckdb/queries.sql` `274ffe1c4f83baad2fc177bbb6773bbdab0db779faeb2970de8cc531292a5dc6`; `duckdb/create.sql` `8065807651b163ab7006b8e451d820e2f6ddc757a6c6bd033800fe1dbc3f81e7`.
- Los dialectos difieren solo en Q19 y Q43 (`toDateTime(EventTime)` en la variante Parquet de DuckDB) y en Q28–Q29 (`STRLEN` en DuckDB, `length` en ClickHouse). La variante nativa `duckdb/` usa el texto de ClickHouse salvo `STRLEN`, porque convierte `EventTime` a TIMESTAMP al cargar.
- Clave primaria de ClickHouse: `PRIMARY KEY (CounterID, EventDate, UserID, EventTime, WatchID)`.
- En el subconjunto, Q20 y Q42 devuelven 0 filas en ambos oráculos; Q37–Q41 y Q43 devuelven filas (existe `CounterID = 62`).

## 5. Línea base de las referencias en esta máquina [medido]

ClickBench sobre `hits_0..hits_9`, mejor de 3 corridas en caliente (no se puede vaciar la caché de páginas sin root), tiempo reportado por cada motor (`--time` en ClickHouse, `.timer` en DuckDB):

- Carga en ClickHouse (MergeTree, `create.sql` oficial): 13.9 s; `OPTIMIZE TABLE hits FINAL`: 13.7 s; 1 parte, `bytes_on_disk` = 1 474 779 179, `data_uncompressed_bytes` = 6 162 360 924.
- Carga en DuckDB nativo (`duckdb/create.sql` + conversión de tiempos): 18.4 s; archivo de 2 394 697 728 bytes.
- Tiempo por consulta (s), ClickHouse / DuckDB: Q1 0.003/0.003, Q2 0.003/0.008, Q3 0.011/0.011, Q4 0.015/0.011, Q5 0.069/0.044, Q6 0.111/0.107, Q7 0.014/0.004, Q8 0.020/0.008, Q9 0.068/0.055, Q10 0.086/0.076, Q11 0.028/0.025, Q12 0.029/0.026, Q13 0.114/0.119, Q14 0.147/0.199, Q15 0.117/0.131, Q16 0.036/0.053, Q17 0.272/0.169, Q18 0.161/0.133, Q19 0.424/0.294, Q20 0.010/0.006, Q21 0.124/0.117, Q22 0.195/0.144, Q23 0.706/0.284, Q24 0.200/0.131, Q25 0.025/0.009, Q26 0.059/0.028, Q27 0.030/0.010, Q28 0.026/0.131, Q29 0.699/1.085, Q30 0.017/0.016, Q31 0.078/0.087, Q32 0.105/0.122, Q33 0.548/0.497, Q34 0.483/0.586, Q35 0.500/0.617, Q36 0.039/0.050, Q37 0.071/0.038, Q38 0.035/0.016, Q39 0.034/0.020, Q40 0.167/0.084, Q41 0.018/0.013, Q42 0.017/0.011, Q43 0.016/0.012.
- RSS máximo por proceso: ClickHouse 285–1 181 MB (Q34), DuckDB 60–1 475 MB (Q34).
- ClickBench calcula su tiempo relativo como `(0.01 + t) / (0.01 + t_base)` (`const constant_time_add = 0.01` en `index.html`, commit fijado).

## 6. Discrepancias entre oráculos encontradas [medido]

| Caso | ClickHouse | DuckDB | Resolución (contrato §15.4) |
|---|---|---|---|
| ClickBench Q4 `AVG(UserID)` | −653 315 757 734.8666: la suma en Int64 se desborda | 2.5131007489380997e18 | Para ClickHouse se usa `AVG(toInt128(UserID))`, que da 2513100748938099700 |
| ClickBench Q29 `REGEXP_REPLACE` | `.` coincide con saltos de línea | `.` no los coincide (RE2 por omisión); 22 Referer con `\n` cambian de grupo | Para DuckDB se antepone `(?s)` al patrón; así ambos dan 15/15 filas iguales |
| TPC-H Q13 (LEFT JOIN + `count(o_orderkey)`) | Con `join_use_nulls=0` rellena con 0 y da `1→501` | `0→500, 1→1` (oficial) | ClickHouse siempre corre con `--join_use_nulls 1`, que da la respuesta oficial |
| Formato | TSV con escapes `\b \f \r \n \t \0 \' \\`; NULL `\N` | CSV con comillas | El arnés deshace los escapes y parte las filas solo por `\n` (Python `splitlines()` corta también en `\r`, `\x1c`…, y rompió Q24 en la prueba) |
| Empates | Q18 (LIMIT sin ORDER BY), Q22, Q32, Q33, Q39, Q40, Q41: mismas claves de orden, distintas filas en el borde | — | Reglas de empate del contrato §15.3 |

Las 43 restantes coinciden fila por fila o como multiconjunto.

## 7. Huecos de la investigación adjunta, cerrados

- Tamaño de `hits.parquet` y de cada `hits_N.parquet`: §4.
- Lista exacta de SQL y funciones: se extrajo de los archivos fijados (contrato §10).
- Si DuckDB y ClickHouse escriben cada codificación: §3. DELTA_BYTE_ARRAY no la escribe ninguno; ambos la leen.
- Si la extensión tpch viene incluida: no; necesita red una vez (§3).
- Repo oficial de ClickBench: se leyó directamente en el commit fijado, no copias.
- Cómo exponen las referencias filas o gránulos leídos: no se usa (decisión 21 del contrato).

## 8. No verificado

- Si la vacante acepta candidatos fuera de EE. UU. (la API dice `Remote`, país USA) [hipótesis: probablemente no].
- DataFusion y Velox: no se consultaron; el proyecto no depende de ellos.
- Cómo hace ClickHouse atómica la publicación de una parte por dentro: no se leyó su código; el diseño del contrato §6 es propio.
- Rendimiento del motor: no existe todavía; ningún umbral del contrato §16 está medido contra él.
