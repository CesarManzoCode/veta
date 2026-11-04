# Cambios

## Sin publicar

### K1 — almacenamiento y ClickBench (en curso)

- Workspace de Cargo con los siete crates del grafo de §2 y `Cargo.lock` sin dependencias externas.
- `veta-core`: tipos, valores con orden total, mapa de validez, vectores y chunks, errores con código de salida, xxh64, CRC32C (SSE4.2 y tabla), CRC-32, HyperLogLog p = 12, PRNG determinista, fechas, decimales, formato TSV, estadísticas, filtros de escaneo y contador de memoria. Implementado; sin verificar todavía.
- `veta-parquet`: decodificadores (PLAIN, RLE/bit-packing híbrido, diccionario, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT) y lector `ParquetFile` de esquemas planos con páginas v1/v2, diccionario, CRC, Snappy, iterador de chunks de 8192 filas y estadísticas por grupo de filas. Implementado; sin verificar todavía.
- `veta-storage`: bloques de §5.3, partes de §5.2 (`PartWriter`, `PartReader` con las verificaciones de apertura de §6.4 y lectura de gránulos verificada contra sus marcas), estadísticas por parte con HLL por prefijo de la clave y muestra sistemática, tablas con `table.sql`, `flock`, inserción atómica de §6.1 ordenada por la clave, instantánea de lector de §6.3 y recuperación de §6.5 con separación de partes rotas a `detached/`. Implementado; sin verificar todavía.
- `veta-exec`: base de datos sobre `--db` con `CREATE [OR REPLACE] TABLE`, pipelines con despachador atómico de morsels sobre `std::thread::scope`, escaneo de tablas (16 gránulos por morsel) y de Parquet (un grupo de filas por morsel), filtro con selección, proyección, agregación hash con tablas por hilo y 64 particiones radix, `COUNT(DISTINCT)` particionado, orden por corridas y mezcla k-way, top-N por montículos, límite con parada temprana, contador de memoria y contadores de `VETA_STATS`; carga de Parquet y TSV por lotes. Implementado; sin verificar todavía.
- Binario `veta`: `sql`, `load`, `parts`, `recover`, `check` (verifica todos los bloques; todavía no separa las partes corruptas), `codec-roundtrip`, `dump-columns` y `codec-bench`. `--no-optimizer`, `--join-order` y `--no-prune` se aceptan y por ahora no cambian nada. `merge`, `merger`, `export`, `parquet-meta` y `--explain-analyze` responden `unsupported` (exit 1). Implementado; sin verificar todavía.

### K0 — fundación (2026-11-01)

- Contrato del motor y del arnés (`vault/CONTRACT.md`): formato de partes, publicación atómica, fusión, códecs, Parquet, SQL, optimizador, ejecución, CLI, comandos de verificación y umbrales.
- Investigación con fuentes en revisiones fijadas y línea base medida de ClickHouse 26.8.34.13 y DuckDB 2.0.0 (`vault/RESEARCH.md`).
- Decisión: DuckDB 2.0.0 (LTS) y ClickHouse 26.8.34.13-lts como oráculos, `hits_0..hits_9` y TPC-H SF1/SF10 como datos.
- Decisión: tres reescrituras por discrepancias medidas entre oráculos (ClickBench Q4 y Q29, `join_use_nulls` en TPC-H).
- `make check` como comando único y CI que lo corre.
