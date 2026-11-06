# Límites

Fuente única de lo que veta no demuestra. Ningún otro archivo repite estos límites.

## Alcance del producto

- Un solo nodo y un solo proceso por consulta. No hay clústeres, replicación, Keeper/Zookeeper, servidor de red ni concurrencia de producción.
- No demuestra Terraform, Ansible, Kubernetes, AWS ni operación de clústeres, que pide la vacante.
- No hay particiones: todas las partes pertenecen a una partición única.
- No hay `INSERT`, `UPDATE` ni `DELETE` en SQL; los datos entran con `veta load` desde Parquet o TSV.
- No hay derrame a disco: una consulta que pasa el límite de memoria falla con exit 4.
- Un lector mantiene abierto un descriptor por cada columna leída de cada parte activa. `veta` sube su límite blando de archivos abiertos al duro (reejecutándose una vez con `ulimit` de `/bin/sh`); si el límite duro no alcanza, la consulta falla con un error de E/S `Too many open files`. La subida solo ocurre en Linux con `/proc/self/limits` legible y `/bin/sh`, y no ocurre si el límite duro es `unlimited`; entonces `veta` sigue con el límite blando que hereda.
- Una fusión mantiene abierto un descriptor por cada columna de cada parte fuente (hasta 10 partes) más uno por cada columna de la parte nueva; le aplica el mismo límite de archivos abiertos que a un lector.
- `merge --final` no ignora `--merge-memory-mb` (§7 solo le quita el puntaje y `--max-merge-rows`): si las partes restantes no caben en ese límite, termina con error 1 y deja más de una parte activa.
- La memoria estimada de una clave con cadenas usa la longitud media de la muestra de cada parte, regla de diseño propia y sin medir: §7 no fija cómo se cuentan sus bytes. La fusión no pasa por el contador de memoria de §12.
- Una fusión escribe sus columnas en un solo hilo.
- El SQL es el subconjunto que exigen las 65 consultas de ClickBench y TPC-H (sin `UNION`, ventanas, `INTERVAL`, tipos anidados ni funciones fuera de esa lista).
- Subconsultas: solo las formas de §10.4. Una subconsulta solo puede referenciar a la consulta inmediatamente superior y solo desde las conjunciones de su `WHERE`. Responden `unsupported`: `NOT IN` correlacionado, `EXISTS` correlacionado sin una igualdad con la consulta superior, escalar correlacionada sin agregado, con `GROUP BY` o con una correlación que no sea igualdad, subconsultas correlacionadas en `HAVING` o en la lista SELECT de una consulta agregada, `IN`/`EXISTS` fuera de una conjunción de `WHERE` o `HAVING` y subconsultas en `ON` o `GROUP BY`.
- El lado que construye un join se concatena en un solo chunk: admite menos de 2³² − 1 filas y hasta 4 GiB por columna de cadenas; si pasa de ahí, la consulta falla. Un anti join consciente de NULL (`NOT IN`) solo admite una clave y ningún residual.
- Un join sin clave de igualdad es un bucle anidado sin tope ni aviso: su trabajo crece con `|L|·|R|`. Antes de sacar las conjunciones comunes de los `OR`, TPC-H Q19 en SF1 (6 M × 200 K filas) no terminó y el arnés mató la consulta (exit −15); cualquier otro join que siga sin clave se expone a lo mismo.
- En `EXPLAIN`, el valor de una subconsulta escalar aparece en `predicates` como la columna `$subqueryN`, que no es SQL válido para DuckDB como pide §11.6.
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
- `cardinality` solo mide el error q de los joins internos cuyo subárbol tiene `Scan`, `Filter`, `Project` y joins internos; los nodos por encima de un agregado o de un join `Left`, `Semi`, `Anti`, `NullAwareAnti` o `Single` no se miden.
- TPC-H SF10 no tiene respuestas oficiales en `tpch_answers()`: `tpch-verify --sf 10` solo compara con DuckDB, que además genera esos datos.
- §11.5 no fija la estimación de los joins `Semi`, `Anti`, `NullAwareAnti`, `Left` y `Single`, ni la de una tabla sin estadísticas o un predicado que mezcla relaciones: veta usa reglas de diseño propias (contención de NDV, 1000 filas, 0.1 por conjunción salvo un `OR` que mezcla relaciones, que se estima rama por rama y combina las ramas como independientes), documentadas en `crates/veta-sql/src/cardinality.rs` y sin medir.
- El binder solo saca de un `OR` las conjunciones idénticas en todas sus ramas, con las columnas escritas igual: `a = b` en una rama y `b = a` en otra no se reconocen, y ese `OR` sigue siendo un predicado de join sin clave.
- La regla 3 cuesta un join sin clave con la misma fórmula que un hash join, aunque su trabajo real crece con `|L|·|R|`; un árbol de más de 64 relaciones conserva el orden del binder. Ninguna de las dos decisiones está medida.
- Sacar las conjunciones comunes de un `OR` corre también con `--no-optimizer`: ese modo no es un plan libre de reescrituras, y `optimizer-oracle` no puede detectar un error de esa reescritura.
- `tlp` y `optimizer-oracle` solo comparan a veta consigo mismo: un error que afecte por igual a la consulta y a sus particiones, o a los dos modos del optimizador, no se detecta.
- Al cierre de K2, `tlp` y `optimizer-oracle` corrieron justo el umbral (1000 y 1065 consultas) y con la semilla por omisión: no hay medición con otras semillas ni con más consultas.
- Las consultas aleatorias no generan subconsultas, `HAVING`, `AVG`, `COUNT(DISTINCT)`, `Float64` ni `Timestamp`, y solo dos tablas pueden unirse por cadenas o sin clave de igualdad. TLP tampoco ejercita `TopN` en las particiones: quita `ORDER BY` y `LIMIT` y ordena en el arnés.
- `kill -9` deja la caché de páginas intacta: los cortes no prueban pérdida de energía, aunque el protocolo hace `fsync`.
- Un byte cambiado en un `c<N>.bin` solo se detecta al leer ese bloque: una consulta que no lee esa columna, o cuya poda salta ese gránulo, responde sin enterarse. Solo `veta check` recorre todos los bloques.
- Una parte separada a `detached/` no vuelve sola y no hay comando para reincorporarla: sus filas salen de la tabla. Si es una parte fusionada cuyas fuentes ya se borraron, no queda otra copia de esas filas.
- `dump-columns` que termina con 3 deja en `--out` los `.raw` que alcanzó a escribir, con las filas de los bloques que sí verificaron.
- DELTA_BYTE_ARRAY no la escribe ningún oráculo: su lectura se prueba con dos archivos de `apache/parquet-testing` y su escritura con lo que leen DuckDB y ClickHouse.
- El catálogo de mutantes tiene seis fallas plantadas; que todas mueran no prueba que el arnés detecte cualquier falla.
- `make check` solo vuelve fatales los avisos `clippy::correctness` y `clippy::suspicious`; al cierre de K2 quedan 19 avisos de estilo y complejidad sin corregir.
- La comparación de ClickBench al cierre de K1 se midió con la tabla `hits` de veta en 10 partes (una por archivo), sin fusiones; el resultado sobre una sola parte fusionada es criterio de K3 (`merge-equivalence`).
- El log de cierre de K2 solo registra el veredicto de cada criterio. Las cifras de TPC-H, `tlp`, `optimizer-oracle`, `join-reorder` y `cardinality` que se publican vienen de la corrida del fixer sobre el mismo commit, no de la corrida de cierre.
- La afirmación de que el proyecto es más difícil que otros proyectos de César no tiene criterio automático.
