# Límites

Fuente única de lo que veta no demuestra. Ningún otro archivo repite estos límites.

## Alcance del producto

- Un solo nodo y un solo proceso por consulta. No hay clústeres, replicación, Keeper/Zookeeper, servidor de red ni concurrencia de producción.
- No demuestra Terraform, Ansible, Kubernetes, AWS ni operación de clústeres, que pide la vacante.
- No hay particiones: todas las partes pertenecen a una partición única.
- No hay `INSERT`, `UPDATE` ni `DELETE` en SQL; los datos entran con `veta load` desde Parquet o TSV.
- No hay derrame a disco: una consulta que pasa el límite de memoria falla con exit 4.
- Un lector mantiene abierto un descriptor por cada columna leída de cada parte activa. `veta` sube su límite blando de archivos abiertos al duro (reejecutándose una vez con `ulimit` de `/bin/sh`); si el límite duro no alcanza, la consulta falla con un error de E/S `Too many open files`. La subida solo ocurre en Linux con `/proc/self/limits` legible y `/bin/sh`, y no ocurre si el límite duro es `unlimited`; entonces `veta` sigue con el límite blando que hereda.
- El SQL es el subconjunto que exigen las 65 consultas de ClickBench y TPC-H (sin `UNION`, ventanas, `INTERVAL`, tipos anidados ni funciones fuera de esa lista).
- Subconsultas: solo las formas de §10.4. Una subconsulta solo puede referenciar a la consulta inmediatamente superior y solo desde las conjunciones de su `WHERE`. Responden `unsupported`: `NOT IN` correlacionado, `EXISTS` correlacionado sin una igualdad con la consulta superior, escalar correlacionada sin agregado, con `GROUP BY` o con una correlación que no sea igualdad, subconsultas correlacionadas en `HAVING` o en la lista SELECT de una consulta agregada, `IN`/`EXISTS` fuera de una conjunción de `WHERE` o `HAVING` y subconsultas en `ON` o `GROUP BY`.
- El lado que construye un join se concatena en un solo chunk: admite menos de 2³² − 1 filas y hasta 4 GiB por columna de cadenas; si pasa de ahí, la consulta falla. Un anti join consciente de NULL (`NOT IN`) solo admite una clave y ningún residual.
- Las cadenas son bytes: `length` y `substring` cuentan bytes, no caracteres.
- `Timestamp` tiene resolución de segundos.
- Parquet: sin tipos anidados ni REPEATED, sin INT96, sin cifrado, sin bloom filters ni page index; compresión solo UNCOMPRESSED y SNAPPY (no ZSTD, GZIP, LZ4, LZ4_RAW ni BROTLI), así que no lee los Parquet por omisión de ClickHouse (zstd). Una página con niveles en BIT_PACKED es un error; solo se tolera que un chunk la liste en sus codificaciones.

## Alcance de la evidencia

- Todas las mediciones son de una máquina (Ryzen 5 5600G, 6 núcleos, 14 GiB visibles) y no se comparan con resultados publicados de ClickBench.
- Sin root no se vacía la caché de páginas: todos los tiempos son en caliente.
- El tiempo de veta es reloj de pared del proceso (incluye arranque); el de ClickHouse y DuckDB es el que reporta cada uno. La comparación es conservadora contra veta, pero no es la misma medida.
- ClickBench usa 10 de 100 archivos (10 M filas); TPC-H llega a SF10. No se prueba a escala de `hits` completo ni de petabytes.
- En el subconjunto, ClickBench Q20 y Q42 devuelven 0 filas en ambos oráculos, así que esas dos solo prueban el resultado vacío.
- ClickBench Q4 y Q29 se comparan contra una reescritura en un oráculo (ClickHouse desborda `AVG` en Int64; DuckDB no hace coincidir `.` con `\n`); veta corre el texto original.
- En los empates del borde de `LIMIT` se comparan las claves de orden, no las demás columnas de esas filas.
- `--tamper` cambia la primera celda de la primera fila de la referencia: si esa celda no es clave de orden y su fila cae en un grupo de empates que corta un `OFFSET` (o un `LIMIT` cuando el grupo llena la ventana), el cambio queda dentro de lo que el borde de `LIMIT` admite y no se detecta; con un resultado de referencia vacío no hay celda que cambiar y la consulta se informa como error.
- La estimación de cardinalidad se mide con TPC-H, cuyos datos son uniformes e independientes, y con una suite sintética de NULL; no con datos correlacionados como el Join Order Benchmark.
- §11.5 no fija la estimación de los joins `Semi`, `Anti`, `NullAwareAnti`, `Left` y `Single`, ni la de una tabla sin estadísticas o un predicado que mezcla relaciones: veta usa reglas de diseño propias (contención de NDV, 1000 filas, 0.1 por conjunción), documentadas en `crates/veta-sql/src/cardinality.rs` y sin medir.
- La regla 3 cuesta un join sin clave con la misma fórmula que un hash join, aunque su trabajo real crece con `|L|·|R|`; un árbol de más de 64 relaciones conserva el orden del binder. Ninguna de las dos decisiones está medida.
- `kill -9` deja la caché de páginas intacta: los cortes no prueban pérdida de energía, aunque el protocolo hace `fsync`.
- DELTA_BYTE_ARRAY no la escribe ningún oráculo: su lectura se prueba con dos archivos de `apache/parquet-testing` y su escritura con lo que leen DuckDB y ClickHouse.
- El catálogo de mutantes tiene seis fallas plantadas; que todas mueran no prueba que el arnés detecte cualquier falla.
- `make check` solo vuelve fatales los avisos `clippy::correctness` y `clippy::suspicious`; al cierre de K1 quedan 16 avisos de estilo y complejidad sin corregir.
- La comparación de ClickBench al cierre de K1 se midió con la tabla `hits` de veta en 10 partes (una por archivo), sin fusiones; el resultado sobre una sola parte fusionada es criterio de K3 (`merge-equivalence`).
- La afirmación de que el proyecto es más difícil que otros proyectos de César no tiene criterio automático.
