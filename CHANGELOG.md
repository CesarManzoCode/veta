# Cambios

## Sin publicar

### K2 — subconsultas, joins, optimizador con cardinalidad, TPC-H y oráculos (2026-11-04)

Implementado y sin medir: ningún comando de este K se ha corrido todavía; los criterios los verifica el cierre del K.

- Subconsultas desanidadas en el binder: semi, anti, anti consciente de NULL y `single` (§10.4).
- Joins hash particionados y paralelos, y joins de bucle anidado para los que no tienen clave.
- Estadísticas de tabla y estimador de cardinalidad (§11.5).
- Reordenación de joins internos por costo (DPccp, voraz por encima de 12 relaciones), `--join-order` y `EXPLAIN ANALYZE`.
- Arnés: `load tpch`, `tpch-verify`, `join-reorder` y `cardinality`.
- Arnés: `gen.py` genera tablas y consultas aleatorias con semilla fija (§17). `tlp` compara cada consulta con la unión de sus particiones `p`, `NOT p` y `p IS NULL`; `optimizer-oracle` compara con y sin `--no-optimizer` esas consultas, las 43 de ClickBench y las 22 de TPC-H. Un error del motor en cualquier ejecución cuenta como discrepancia.
- Decisión: al partir `WHERE` y `ON` en conjunciones, el binder saca de cada `OR` las conjunciones que repiten todas sus ramas (`(a AND b) OR (a AND c)` → `a`, `b OR c`; es una identidad de la lógica de tres valores). Así la igualdad de TPC-H Q19, escrita en cada rama, se vuelve clave de join al construir el plan (§11.1) también con `--no-optimizer`; sin esto Q19 era un join de bucle anidado de 6 M × 200 K filas en SF1, que no terminaba.
- Decisión: el estimador ya no da 0.1 a un `OR` que mezcla relaciones; estima cada rama como un filtro (muestra de cada tabla) y combina las ramas como independientes, `1 − Π (1 − s_i)`. Con 0.1, el join de Q19 en SF1 se estimaba en 22 352 filas contra 121 reales (error q 185).
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
