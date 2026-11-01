# Cambios

## Sin publicar

### K0 — fundación (2026-11-01)

- Contrato del motor y del arnés (`vault/CONTRACT.md`): formato de partes, publicación atómica, fusión, códecs, Parquet, SQL, optimizador, ejecución, CLI, comandos de verificación y umbrales.
- Investigación con fuentes en revisiones fijadas y línea base medida de ClickHouse 26.8.34.13 y DuckDB 2.0.0 (`vault/RESEARCH.md`).
- Decisión: DuckDB 2.0.0 (LTS) y ClickHouse 26.8.34.13-lts como oráculos, `hits_0..hits_9` y TPC-H SF1/SF10 como datos.
- Decisión: tres reescrituras por discrepancias medidas entre oráculos (ClickBench Q4 y Q29, `join_use_nulls` en TPC-H).
- `make check` como comando único y CI que lo corre.
