# Cambios

## Sin publicar

### K3 — fusión, recuperación, escritura Parquet y comandos de durabilidad, Parquet y compresión (sin fecha de cierre)

Nada de esta entrada está medido todavía: los comandos existen y tienen tests con datos sintéticos, pero ninguno se corrió contra el binario real en sus mini sprints. Las cifras y los veredictos llegan con la verificación de cierre del K.

- Fusión vertical y política de ventanas de §7: `veta merge [--final]` y el fusionador en segundo plano `veta merger`.
- Recuperación y detección de corrupción: `veta check` verifica cada bloque de cada parte, separa las corruptas en `detached/` y termina con 3; las lecturas, `dump-columns` y `codec-roundtrip --db` hacen lo mismo con una parte rota.
- Escritura Parquet propia: `veta export` con `--encoding plain|dict|delta|delta-byte-array`, `--compression none|snappy`, `--page-version 1|2` y `--row-group-rows`, con estadísticas exactas por column chunk. `veta parquet-meta` imprime el pie en TSV.
- Arnés de durabilidad: `merge-equivalence`, `concurrent-reader`, `crash-merge`, `crash-insert` y `corruption`.
- Arnés de Parquet (`parquet_interop.py`): `parquet-read-duckdb` (archivos de DuckDB en v1/v2, con y sin diccionario, sin comprimir y Snappy, con columnas nullable y `DOUBLE`, más los dos de parquet-testing; cobertura de codificaciones por `parquet_metadata`), `parquet-read-hits`, `parquet-write-duckdb` y `parquet-write-clickhouse` (`EXCEPT ALL` en ambos sentidos contra la tabla del oráculo, con cada columna convertida al tipo de esa tabla), `parquet-stats` y `parquet-rowgroup-skip`.
- Arnés de compresión: `disk-bytes` (`merge --final`, `du -sb` de `hits` frente a `sum(bytes_on_disk)` de ClickHouse) y `compressor` (`vlz` frente a `lz4 -b1 -i3 -q` sobre `dump-columns` de `hits`, con ratio y velocidad de descompresión agregados de §16 y los números crudos por archivo en `report.json`).
- Decisión: `parquet-read-duckdb` lee muestras y no tablas completas, porque compara multiconjuntos en memoria: 20 000 filas de `hits` (todas sus columnas) y las filas de `lineitem` SF1 con `l_orderkey <= 40000`, escritas en grupos de 10 240 filas.
- Decisión: `parquet-read-hits` suma las longitudes en bytes, con `length` en veta y `strlen` en DuckDB, porque el `length` de DuckDB cuenta caracteres.
- Descompresión de `vlz` más rápida en coincidencias de desplazamiento menor que 16: escribe byte a byte solo el primer múltiplo del desplazamiento que llega a 16 y copia el resto en bloques de 16 bytes. Antes toda la coincidencia se copiaba byte a byte.
- Decisión: `codec-bench` descomprime en un mismo buffer de salida en todas sus corridas (`vlz::decompress_into`, `snappy::decompress_into`), como hace `lz4 -b`. Así mide el decodificador y no los fallos de página de alocar en cada corrida una salida nueva, que en `Title` de `hits` es de 1.1 GB.
- Decisión: cada comando escribe sus propios archivos en `scratch/<nombre>/`; `parquet-write-duckdb`, `parquet-write-clickhouse` y `parquet-stats` repiten las mismas 66 exportaciones en vez de compartirlas.

### K2 — subconsultas, joins, optimizador con cardinalidad, TPC-H y oráculos (2026-11-06)

- Las 22 consultas de TPC-H devuelven lo correcto: en SF1 iguales a `tpch_answers()` y en SF10 iguales a DuckDB 2.0.0. Verificado con `tpch-verify --sf 1|10`: 22/22 iguales en ambos.
- Subconsultas escalares, `IN`, `NOT IN`, `EXISTS` y `NOT EXISTS`, correlacionadas o no, en las formas de §10.4: el binder las reescribe como joins semi, anti, anti consciente de NULL y `single`.
- Joins hash particionados y paralelos, y joins de bucle anidado para los que no tienen clave.
- Estadísticas de tabla y estimador de cardinalidad (§11.5). Verificado con `cardinality`: error q con mediana 1.088, p90 4.832 y máximo de la suite de NULL 2.005, con conteos iguales a DuckDB.
- Los joins internos se reordenan por costo (DPccp, voraz por encima de 12 relaciones). Verificado con `join-reorder`: el orden por costo es 1.961 veces más rápido que el sintáctico en la suma de TPC-H SF1, con resultados iguales en las 22.
- `--join-order`, `EXPLAIN` y `EXPLAIN ANALYZE` (`--explain-analyze`) ya funcionan; dejan de responder `unsupported` o de no tener efecto.
- Arnés: `load tpch`, `tpch-verify`, `join-reorder` y `cardinality`.
- Arnés: `gen.py` genera tablas y consultas aleatorias con semilla fija (§17). `tlp` compara cada consulta con la unión de sus particiones `p`, `NOT p` y `p IS NULL`; `optimizer-oracle` compara con y sin `--no-optimizer` esas consultas, las 43 de ClickBench y las 22 de TPC-H. Un error del motor en cualquier ejecución cuenta como discrepancia. Verificado: `tlp` con 1000 consultas y `optimizer-oracle` con 1065, ambos con 0 discrepancias y 0 errores.
- Los criterios de K1 siguen pasando, incluido ClickBench contra DuckDB y ClickHouse. `make check`: 549 tests de Rust y 211 del arnés, 0 fallos.
- Las cifras de este K son de la corrida de `check_k2.py` sobre `9586a99`, el mismo commit que pasó la verificación de cierre.
- Decisión: al partir `WHERE` y `ON` en conjunciones, el binder saca de cada `OR` las conjunciones que repiten todas sus ramas (`(a AND b) OR (a AND c)` → `a`, `b OR c`; es una identidad de la lógica de tres valores). Así la igualdad de TPC-H Q19, escrita en cada rama, se vuelve clave de join al construir el plan (§11.1) también con `--no-optimizer`; sin esto Q19 era un join de bucle anidado de 6 M × 200 K filas en SF1, que no terminaba.
- Decisión: el estimador ya no da 0.1 a un `OR` que mezcla relaciones; estima cada rama como un filtro (muestra de cada tabla) y combina las ramas como independientes, `1 − Π (1 − s_i)`. Con 0.1, el join de Q19 en SF1 se estimaba en 22 352 filas contra 121 reales (error q 185); ahora su error q es 1.42.
- Decisión: el generador cambia de base cada 250 consultas (cada una con sus 3 tablas), para que el número de filas y las fracciones de NULL también varíen dentro de una corrida. Con menos consultas que el umbral de §16, `tlp` y `optimizer-oracle` terminan en FAIL.

### K1 — almacenamiento, códecs, lectura Parquet y ClickBench correcto (2026-11-04)

- Las 43 consultas de ClickBench sobre `hits_0..hits_9` (10 M filas) devuelven lo mismo que DuckDB 2.0.0 y que ClickHouse 26.8.34.13. Verificado con `clickbench-verify --against duckdb|clickhouse`: 43/43 iguales.
- `veta load` carga Parquet y TSV en tablas de partes inmutables ordenadas por clave primaria. Cada lote se publica como una parte atómica, con índice disperso de gránulos de 8192 filas, CRC32C por bloque, estadísticas por parte y muestra. La carga de los 10 Parquet de `hits` está verificada por los 43/43. La recuperación (`veta recover`) y las verificaciones de apertura solo tienen tests unitarios; sus comandos de verificación llegan en K3.
- Lectura de Parquet propia, de esquemas planos: páginas v1/v2, diccionario, PLAIN, RLE/bit-packing, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY y BYTE_STREAM_SPLIT, con compresión Snappy o sin comprimir. `hits_N.parquet` se lee igual que en los oráculos; las demás configuraciones se verifican en K3 (`parquet-read-duckdb`).
- Códecs de columna propios (Plain, FOR con bit-packing, delta, diccionario, RLE), compresor `vlz` y Snappy. Verificado con `codec-roundtrip`: 0 fallos sobre las columnas de `hits` y 2000 columnas aleatorias.
- `veta sql` ejecuta consultas de una tabla vectorizadas y multihilo: filtros, proyección, `GROUP BY` con agregación hash, `COUNT(DISTINCT)`, `ORDER BY`, top-N, `LIMIT`/`OFFSET`, `LIKE`, expresiones regulares y las funciones de ClickBench. También acepta `--threads`, `--stats`, `--no-optimizer`, `--no-prune` y `--memory-limit-mb` (exit 4 al pasarlo).
- Las consultas con filtro sobre la clave leen solo los gránulos que pueden coincidir. Las partes se descartan por rango de clave y mín/máx, y los grupos de filas de Parquet por sus estadísticas. Verificado con `pruning`: en Q37–Q43 se leen menos gránulos con poda que sin ella, con los mismos resultados.
- Optimizador base: plegado de constantes, empuje de predicados (en `LEFT JOIN`, solo al lado preservado), filtros de escaneo, poda de columnas y top-N.
- `veta` sube por sí mismo el límite blando de archivos abiertos para leer muchas partes × columnas.
- Arnés `python3 -m harness`, solo con la biblioteca estándar:
  - `fetch` descarga y verifica las fijaciones del apéndice A;
  - `load clickbench` carga los tres motores;
  - `clickbench-verify` compara con las reglas de §15 y detecta una referencia alterada (`--tamper 8` termina en FAIL);
  - `pruning`, `codec-roundtrip`, `size` y `deps` pasan.
- Responden `unsupported` (exit 1) hasta su K: joins, subconsultas y `EXPLAIN` (K2); `merge`, `merger`, `export`, `parquet-meta` y la separación de partes corruptas en `check` (K3). `--join-order` se acepta sin efecto hasta K2.
- Decisión: el borde de `LIMIT` solo relaja por claves los grupos que la ventana puede cortar. Así `--tamper` también se detecta cuando la primera celda no es clave de orden.

### K0 — fundación (2026-11-01)

- Contrato del motor y del arnés (`vault/CONTRACT.md`): formato de partes, publicación atómica, fusión, códecs, Parquet, SQL, optimizador, ejecución, CLI, comandos de verificación y umbrales.
- Investigación con fuentes en revisiones fijadas y línea base medida de ClickHouse 26.8.34.13 y DuckDB 2.0.0 (`vault/RESEARCH.md`).
- Decisión: DuckDB 2.0.0 (LTS) y ClickHouse 26.8.34.13-lts como oráculos, `hits_0..hits_9` y TPC-H SF1/SF10 como datos.
- Decisión: tres reescrituras por discrepancias medidas entre oráculos (ClickBench Q4 y Q29, `join_use_nulls` en TPC-H).
- `make check` como comando único y CI que lo corre.
