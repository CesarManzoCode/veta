# Cambios

## Sin publicar

### K1 — almacenamiento y ClickBench (en curso)

- Workspace de Cargo con los siete crates del grafo de §2 y `Cargo.lock` sin dependencias externas.
- `veta-core`: tipos, valores con orden total, mapa de validez, vectores y chunks, errores con código de salida, xxh64, CRC32C (SSE4.2 y tabla), CRC-32, HyperLogLog p = 12, PRNG determinista, fechas, decimales, formato TSV, estadísticas, filtros de escaneo y contador de memoria. Implementado; sin verificar todavía.
- `veta-parquet`: decodificadores (PLAIN, RLE/bit-packing híbrido, diccionario, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT) y lector `ParquetFile` de esquemas planos con páginas v1/v2, diccionario, CRC, Snappy, iterador de chunks de 8192 filas y estadísticas por grupo de filas. Implementado; sin verificar todavía.
- `veta-storage`: bloques de §5.3, partes de §5.2 (`PartWriter`, `PartReader` con las verificaciones de apertura de §6.4 y lectura de gránulos verificada contra sus marcas), estadísticas por parte con HLL por prefijo de la clave y muestra sistemática, tablas con `table.sql`, `flock`, inserción atómica de §6.1 ordenada por la clave, instantánea de lector de §6.3 y recuperación de §6.5 con separación de partes rotas a `detached/`. Implementado; sin verificar todavía.
- Binario `veta`: reconoce los subcomandos de §13 y responde `unsupported: <subcomando>` (exit 1).

### K0 — fundación (2026-11-01)

- Contrato del motor y del arnés (`vault/CONTRACT.md`): formato de partes, publicación atómica, fusión, códecs, Parquet, SQL, optimizador, ejecución, CLI, comandos de verificación y umbrales.
- Investigación con fuentes en revisiones fijadas y línea base medida de ClickHouse 26.8.34.13 y DuckDB 2.0.0 (`vault/RESEARCH.md`).
- Decisión: DuckDB 2.0.0 (LTS) y ClickHouse 26.8.34.13-lts como oráculos, `hits_0..hits_9` y TPC-H SF1/SF10 como datos.
- Decisión: tres reescrituras por discrepancias medidas entre oráculos (ClickBench Q4 y Q29, `join_use_nulls` en TPC-H).
- `make check` como comando único y CI que lo corre.
