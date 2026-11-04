# veta

Motor SQL analítico columnar escrito desde cero en Rust: partes inmutables ordenadas por clave primaria con índice disperso, códecs de columna y compresor propios, fusión en segundo plano con recuperación tras `kill -9`, ejecución vectorizada y multihilo, optimizador con estadísticas de cardinalidad, y lectura y escritura propias de Parquet. Su exactitud y su tiempo se miden contra ClickHouse 26.8.34.13 y DuckDB 2.0.0 en la misma máquina, sobre ClickBench (43 consultas, 10 M filas de `hits`) y TPC-H (22 consultas, SF1 y SF10).

Lo que el proyecto no demuestra está en [LIMITATIONS.md](LIMITATIONS.md).

## Estado

| Fase | Contenido | Estado |
|---|---|---|
| K0 | Fundación: contrato, investigación con fuentes fijadas, plan y comando de chequeo | Cerrada |
| K1 | Almacenamiento, códecs, lectura Parquet, SQL de una tabla y ClickBench igual a ClickHouse y DuckDB | Cerrada |
| K2 | Subconsultas, joins, optimizador con cardinalidad, TPC-H y oráculos TLP y encendido/apagado | Pendiente |
| K3 | Fusiones, recuperación tras cortes, detección de corrupción, escritura Parquet y compresión | Pendiente |
| K4 | Rendimiento contra las referencias, memoria, multihilo y catálogo de mutantes | Pendiente |

## Resultados

Medidos al cierre de K1 (2026-11-04), en la máquina del proyecto (Ryzen 5 5600G, 14 GiB visibles):

| Comando | Resultado |
|---|---|
| `clickbench-verify --against duckdb` | PASS, 43/43 iguales a DuckDB 2.0.0 sobre `hits_0..hits_9` (10 M filas) |
| `clickbench-verify --against clickhouse` | PASS, 43/43 iguales a ClickHouse 26.8.34.13 |
| `clickbench-verify --against duckdb --tamper 8` | FAIL, como debe: la comparación detecta la referencia alterada |
| `codec-roundtrip` | PASS, 0 fallos sobre las columnas de `hits` y 2000 columnas aleatorias (semilla 1) |
| `pruning` | PASS: en Q37–Q43 se leen menos gránulos con poda que sin ella, con los mismos resultados |
| `deps` | PASS: `Cargo.lock` solo con paquetes `veta-*`; el arnés solo usa la biblioteca estándar |
| `size` | PASS: el arnés tiene menos líneas que el motor |
| `make check` | 472 tests de Rust y 106 del arnés, 0 fallos |

Todavía no hay resultados de tiempo, memoria ni multihilo de veta (K4). La línea base de las referencias está en `vault/RESEARCH.md` §5.

## Uso

```sh
make check                      # formato, clippy, tests de Rust y del arnés
python3 -m harness fetch        # descarga única de binarios y datos fijados (necesita red)
python3 -m harness all          # todos los criterios; 77 = falta un binario de referencia
```

El diseño y las decisiones están en `vault/CONTRACT.md`.

## Licencia

Apache-2.0. Las consultas y esquemas de ClickBench (CC BY-NC-SA 4.0) y las consultas TPC-H de DuckDB (MIT) no se distribuyen aquí: el arnés las descarga en revisiones fijadas.
