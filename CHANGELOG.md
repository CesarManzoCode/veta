# Cambios

## Sin publicar

### K4 — rendimiento, memoria, multihilo, mutantes y verificación total (sin fecha de cierre)

Nada de esta entrada está medido todavía: ningún mini sprint compiló, corrió ni midió su código. Las mejoras de velocidad son hipótesis, y las cifras y los veredictos llegan con la verificación de cierre del K.

- Ejecutor más rápido (hipótesis de diseño): agrupación por índice directo cuando las estadísticas acotan una clave entera, agrupación por código de diccionario en cadenas, filtros evaluados una vez por entrada de diccionario, materialización tardía en top-N, conjunciones evaluadas de la más barata a la más cara, `LIKE '%lit%'` con SSE2/AVX2 y prefiltro de prefijo literal en expresiones regulares. Cada cambio tiene un test de equivalencia contra el camino general.
- Arnés de rendimiento: `perf clickbench|tpch` (3 corridas por consulta en los tres motores, `runs/perf-<b>/times.csv` y razones por consulta y media geométrica frente a cada referencia), `rss` (tope de memoria de las 65 consultas con `/usr/bin/time -f %M`) y `threads` (ClickBench con `--threads 1` frente a `--threads 12`).
- Catálogo de mutantes de §18: seis features de Cargo apagadas por omisión (`mutant-prune-extra-granule` y `mutant-prune-wrong-bound` en veta-storage; `mutant-pushdown-outer`, `mutant-reorder-outer` y `mutant-estimate-ignore-null` en veta-sql; `mutant-codec-drop-last-block` en veta-codec), reexportadas por veta-cli. Cada pieza que rompen tiene además un test normal que fallaría con el mutante activo.
- Arnés: `mutants` compila cada mutante en `target/mutants/<nombre>`, corre con él los siete comandos de §18 e imprime `MUTANT <nombre> KILLED by <comandos>` o `MUTANT <nombre> SURVIVED`; `all` corre en orden los comandos de §17 y pasa si cada uno es PASS o SKIP.
- Decisión: para `mutants`, un comando mata al mutante si termina con 1 (FAIL). Un 2 (error de infraestructura) hace que `mutants` termine con 2, porque su veredicto no es fiable. Sin `bin/duckdb`, `mutants` es SKIP.
- Decisión: `mutants` guarda las corridas con mutante en `runs/mutants/<nombre>/<comando>/` y restaura los `runs/<comando>/` que existían antes. Así, después de `all`, los reportes de los comandos siguen siendo los del binario sin mutar.
- Decisión: `mutants` y `all` muestran la salida de cada subcomando con el prefijo `<comando>| ` y la guardan entera en `runs/<driver>/logs/`. Así la única línea que empieza con `RESULT` es la del propio driver.

### K3 — fusión, recuperación, escritura Parquet y comandos de durabilidad, Parquet y compresión (sin fecha de cierre)

- Las tablas se fusionan: `veta merge [--final]` en primer plano y `veta merger` en segundo plano, con fusión vertical y la política de ventanas de §7. Fusionar no cambia los resultados. Verificado con `merge-equivalence`: las 43 consultas de ClickBench dan lo mismo con `hits` en 100 partes y en una sola parte fusionada.
- Las consultas leen durante las fusiones sin diferencias ni errores. Verificado con `concurrent-reader`: ≥ 200 iteraciones con `merger` corriendo, y el número de partes activas bajó.
- Un `kill -9` en medio de una fusión o de una carga no deja la tabla a medias: tras `veta recover` no queda ningún temporal y cada lote está completo o ausente. Verificado con `crash-merge` y `crash-insert`: ≥ 100 cortes cada uno, ≥ 80 % con el proceso todavía vivo.
- Las partes corruptas se detectan y se separan: `veta check` verifica cada bloque de cada parte, mueve las corruptas a `detached/` y termina con 3. Las consultas, `dump-columns` y `codec-roundtrip --db` hacen lo mismo cuando leen un bloque roto. Verificado con `corruption`: un byte cambiado en cualquiera de los 7 tipos de archivo de una parte la deja en `detached/`.
- La lectura de Parquet ya está verificada con archivos de DuckDB en todas las configuraciones (páginas v1/v2, con y sin diccionario, sin comprimir y Snappy), con los dos archivos DELTA_BYTE_ARRAY de parquet-testing y con los 10 `hits_N.parquet`. Verificado con `parquet-read-duckdb`, que cubre las 8 codificaciones de §9, y con `parquet-read-hits`.
- Escritura Parquet propia: `veta export` con `--encoding plain|dict|delta|delta-byte-array`, `--compression none|snappy`, `--page-version 1|2` y `--row-group-rows`, con mín, máx y conteo de NULL exactos por column chunk. `veta parquet-meta` imprime el pie en TSV. Verificado con `parquet-write-duckdb` y `parquet-write-clickhouse`: DuckDB y ClickHouse leen sin diferencias `hits` y las 8 tablas de TPC-H SF1 exportadas. Las estadísticas coinciden con las que lee y calcula DuckDB (`parquet-stats`).
- `read_parquet` salta los grupos de filas que las estadísticas excluyen. Verificado con `parquet-rowgroup-skip`: en 5 consultas sobre `lineitem` SF1 se leen menos grupos de filas que el total, con el mismo resultado que sin poda.
- Con `hits` en una sola parte, veta ocupa en disco ≤ 1.10 × lo que ocupa ClickHouse (`disk-bytes`).
- El compresor `vlz` comprime más que `lz4 -b1` y descomprime a más de un tercio de su velocidad. Verificado con `compressor` sobre las columnas de `hits`: ratio agregado 6.509 contra 5.008 y descompresión agregada 4091.855 MB/s contra 5523.429 MB/s (0.74×).
- Descompresión de `vlz` más rápida en coincidencias con desplazamiento menor que 16: solo el primer múltiplo del desplazamiento que llega a 16 bytes se copia byte a byte, y el resto en bloques de 16 bytes. Antes se copiaba byte a byte la coincidencia entera. En la columna `CLID`, de ~700 a ~5100 MB/s en `codec-bench`.
- `make check`: todos los tests de Rust y 266 del arnés, 0 fallos.
- Las cifras de este K son de la corrida de `check_k3.py` sobre `02c6d83`, el mismo árbol que pasó la verificación de cierre.
- Decisión: `.insert.lock` se suelta después de bloquear el temporal de la inserción, no antes como dice §6.1, y una fusión no forma ventanas que crucen un bloque con inserción viva. Así dos cargas concurrentes y una fusión no pueden publicar una parte ya cubierta, que perdería sus filas.
- Decisión: la recuperación borra temporales bajo `.insert.lock` exclusivo, y verifica cada bloque de la parte que cubre antes de borrar las partes cubiertas. Así nunca borra trabajo en curso ni la única copia intacta de unas filas.
- Decisión: `merge --final` respeta `--merge-memory-mb`, porque §7 solo le quita el puntaje y `--max-merge-rows`. Si las partes no caben, termina con error 1.
- Decisión: `parquet-read-duckdb` lee muestras y no tablas completas, porque compara multiconjuntos en memoria: 20 000 filas de `hits` (todas sus columnas) y las filas de `lineitem` SF1 con `l_orderkey <= 40000`, escritas en grupos de 10 240 filas.
- Decisión: `parquet-read-hits` suma las longitudes en bytes, con `length` en veta y `strlen` en DuckDB, porque el `length` de DuckDB cuenta caracteres.
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
