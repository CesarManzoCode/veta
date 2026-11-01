# Contrato — veta

Constitución del proyecto: qué debe hacer el sistema y todas las decisiones cerradas en K0. Las cifras medidas y las fuentes con revisión fijada están en `vault/RESEARCH.md`; los límites, solo en `LIMITATIONS.md`. Este documento es **diseño aceptado**: nada de lo que dice está implementado al cerrar K0.

## 1. Qué es

veta es un motor SQL analítico columnar escrito desde cero en Rust. Guarda tablas como partes inmutables ordenadas por clave primaria con índice disperso, comprime cada columna con códecs propios, fusiona partes en segundo plano, se recupera de un `kill -9`, ejecuta consultas vectorizadas y multihilo, optimiza con estadísticas de cardinalidad y lee y escribe Parquet con implementación propia. Un arnés en Python compara su exactitud y su tiempo contra ClickHouse y DuckDB en la misma máquina, sobre ClickBench (43 consultas) y TPC-H (22 consultas).

Se demuestra todo con comandos externos (§15–§17): cada criterio de la idea es un comando del arnés que termina con 0, 1 o 77.

## 2. Estructura del repositorio

```
Cargo.toml                 workspace (resolver "3", edition 2024), sin dependencias externas
rust-toolchain.toml        channel "1.98.1", components ["rustfmt", "clippy"]
crates/veta-core/          tipos, valores, vectores, errores, hashing, crc32c, HLL, PRNG, formato de salida
crates/veta-codec/         codificaciones de columna, compresor vlz, Snappy
crates/veta-parquet/       Thrift compacto, metadatos, lector y escritor Parquet
crates/veta-storage/       formato de partes, tablas, inserción, poda, fusión, recuperación
crates/veta-sql/           lexer, parser, AST, binder, plan lógico, optimizador, estadísticas
crates/veta-exec/          expresiones, funciones, regex/LIKE, operadores, planificador de hilos
crates/veta-cli/           binario `veta` (§13)
harness/                   arnés de verificación, paquete Python (§14); tests en harness/tests/
bench/tpch/create.sql      DDL de TPC-H del proyecto (§14.3)
Makefile                   `make check`, el comando único de chequeo
```

Grafo de dependencias entre crates (Cargo lo hace cumplir; ninguna otra arista está permitida):

```
veta-core    ← (nada)
veta-codec   ← core
veta-parquet ← core, codec
veta-storage ← core, codec
veta-sql     ← core
veta-exec    ← core, codec, parquet, storage, sql
veta-cli     ← todos
```

Los tests de Rust van en el mismo archivo (`#[cfg(test)] mod tests`) o en `crates/<crate>/tests/`. `unsafe` solo se permite en `veta-codec` y `veta-exec`, en bucles calientes, con un comentario `// SAFETY:` por bloque.

## 3. Reglas de dependencias

- El motor no tiene ninguna dependencia fuera del workspace: `Cargo.lock` solo contiene paquetes `veta-*` sin campo `source`. Se usa solo `std` (incluido `std::arch` para CRC32C y SIMD).
- El arnés usa solo la biblioteca estándar de Python ≥ 3.12 (`sys.stdlib_module_names`) y su propio paquete `harness`.
- DuckDB y ClickHouse son binarios externos usados solo como oráculos y referencias de tiempo; `lz4` del sistema solo como referencia del compresor.

## 4. Tipos y semántica de valores

| Tipo SQL | `DataType` | Representación física |
|---|---|---|
| BOOLEAN | `Boolean` | bits / `bool` |
| SMALLINT, INT2 | `Int16` | `i16` |
| INTEGER, INT, INT4 | `Int32` | `i32` |
| BIGINT, INT8 | `Int64` | `i64` |
| (solo resultado) | `Int128` | `i128`; resultado de `SUM(Int64)`; no se puede declarar en tablas |
| DOUBLE, FLOAT8, DOUBLE PRECISION, FLOAT, REAL | `Float64` | `f64` |
| DECIMAL(p,s), NUMERIC(p,s) | `Decimal { precision, scale }` | `i64` si p ≤ 18, `i128` si 18 < p ≤ 38 |
| VARCHAR[(n)], TEXT, CHAR[(n)], STRING | `Varchar` | bytes arbitrarios (no se exige UTF-8); `n` se ignora |
| DATE | `Date` | `i32`, días desde 1970-01-01 |
| TIMESTAMP, DATETIME | `Timestamp` | `i64`, segundos desde 1970-01-01 00:00:00 UTC |

Toda columna lleva `nullable: bool`; los NULL se guardan en un mapa de validez aparte de los valores.

Semántica (debe coincidir con los oráculos en las 65 consultas):

- Lógica de tres valores. `WHERE` y `HAVING` descartan NULL y falso.
- Aritmética entera: los literales enteros son `Int64`; `Int16`/`Int32` se promueven a `Int64` al operar; el desbordamiento es error (exit 1). `/` entre enteros o decimales da `Float64`. `%` es módulo entero.
- Decimales exactos: suma/resta → escala `max(s1,s2)`, precisión `max(p1−s1, p2−s2) + max(s1,s2) + 1`; multiplicación → escala `s1+s2`, precisión `p1+p2`; precisión tope 38 (si se pasa, error). Un literal `0.05` es `Decimal(3,2)`; entero con decimal → decimal con escala 0. Comparaciones reescalan sin pérdida.
- Agregados: `COUNT` → `Int64`; `SUM(Int16|Int32)` → `Int64`; `SUM(Int64)` → `Int128`; `SUM(Decimal(p,s))` → `Decimal(38,s)`; `SUM(Float64)` → `Float64`; `AVG(entero|decimal)` → `Float64` calculado como suma exacta en `i128` dividida entre el conteo; `MIN`/`MAX` del tipo de entrada. Sin filas: `COUNT` = 0, los demás NULL.
- Cadenas: comparación byte a byte; `length`/`STRLEN` cuentan bytes; `substring` usa posiciones en bytes, base 1.
- Comparar `Date`/`Timestamp` con un literal de cadena convierte la cadena (`'2013-07-01'`, `'2013-07-01 10:00:00'`).
- Regex (`REGEXP_REPLACE`): semántica de ClickHouse; `.` coincide con `\n`; reemplaza todas las coincidencias no solapadas; `\0`–`\9` en el reemplazo son grupos.
- El orden de `ORDER BY` es ascendente por omisión; NULL al final en ASC y al principio en DESC (como DuckDB), salvo `NULLS FIRST|LAST` explícito.

## 5. Formato en disco

### 5.1 Base y tabla

`--db DIR` es un directorio. Cada tabla vive en `DIR/<tabla>/` (nombre en minúsculas):

```
table.sql              CREATE TABLE canónico escrito por el motor; se reparsea al abrir
block_number.txt       marca de agua del último número de bloque asignado (escrito con tmp + rename)
.insert.lock .merge.lock .readers.lock     archivos de flock (§6)
all_<min>_<max>_<level>/                   partes activas o cubiertas
tmp_insert_<b>/ tmp_merge_<nombre>/        partes en construcción (con owner.lock dentro)
delete_tmp_<nombre>/                       partes en borrado
detached/broken_<nombre>/                  partes corruptas separadas
```

No hay particiones: todas las partes pertenecen a la partición única `all`. Una parte `A` **cubre** a `B` si `A.min ≤ B.min`, `B.max ≤ A.max` y `A ≠ B`. Las partes activas son las no cubiertas por otra parte presente.

### 5.2 Parte

Filas ordenadas por la clave primaria (orden no único, como ClickHouse; sin clave = orden de inserción). Gránulo fijo de 8192 filas (el último puede ser menor). Archivos, con columnas por índice de esquema `N` para no escapar nombres:

| Archivo | Contenido |
|---|---|
| `part.txt` | líneas `clave=valor`: `format=1`, `rows`, `granules`, `index_granularity=8192`, `min_block`, `max_block`, `level`, y una línea `column=<N>\t<nombre>\t<tipo SQL>\t<nullable 0/1>` por columna |
| `primary.idx` | un bloque (§5.3) por columna de la clave con `granules + 1` valores: el valor en la primera fila de cada gránulo y, al final, el de la última fila de la parte |
| `c<N>.bin` | un bloque por gránulo, en orden |
| `c<N>.mrk` | `granules + 1` desplazamientos `u64` LE en `c<N>.bin`; el último es la longitud del archivo |
| `stats.bin` | por columna: `rows u64`, `nulls u64`, `has_minmax u8`, `min`, `max` (codificación de valor), registros HLL (p = 12, 4096 bytes); después un HLL por cada prefijo de la clave primaria (longitudes 1..k) |
| `sample.bin` | muestra sistemática de `s = min(1024, rows)` filas (posiciones `floor(i·rows/s)`), un bloque por columna |
| `checksums.txt` | `<archivo>\t<bytes>\t<crc32c hex>` por cada otro archivo de la parte; última línea `self\t<crc32c de las líneas anteriores>` |

`owner.lock` solo existe en directorios `tmp_*` y no se incluye en `checksums.txt`.

### 5.3 Bloque

```
u32 payload_len | u32 rows | u8 encoding | u8 compressor | u8 flags | u8 reserved | u32 crc32c
payload (payload_len bytes)
```

`crc32c` (Castagnoli) cubre los 12 bytes de cabecera anteriores y el payload. `flags & 1`: el payload empieza con el mapa de validez (bits LSB-first, `ceil(rows/8)` bytes). El payload sin comprimir es la salida de la codificación (§8.1); si `compressor = 1` el payload está comprimido con vlz (§8.2) y su primer campo es la longitud descomprimida. Al leer, un bloque cuyo inicio no coincide con su marca, cuya longitud no llega a la siguiente marca, cuyo `rows` no es el del gránulo o cuyo CRC falla es **corrupción** (§6.4).

## 6. Publicación atómica, concurrencia y recuperación

### 6.1 Inserción (un lote = una parte = atómico)

1. `flock` exclusivo de `.insert.lock`; lee `block_number.txt` y el máximo `max_block` presente; `b = max + 1`; escribe `block_number.txt` (tmp, fsync, rename); suelta el lock.
2. Crea `tmp_insert_<b>/` y `flock` exclusivo de `tmp_insert_<b>/owner.lock`, que retiene hasta el final.
3. Escribe todos los archivos, `fsync` de cada uno, `fsync` del directorio.
4. `rename(tmp_insert_<b>, all_<b>_<b>_0)`, `fsync` del directorio de la tabla.

### 6.2 Fusión

1. `flock` exclusivo de `.merge.lock` (un solo fusionador por tabla).
2. Elige partes (§7), escribe `tmp_merge_<nombre>/` con `owner.lock` como en 6.1.
3. `rename` a `all_<min>_<max>_<level_max+1>`, `fsync` del directorio.
4. Limpieza: `flock` exclusivo de `.readers.lock`; cada parte cubierta se renombra a `delete_tmp_<nombre>` y se borra; suelta el lock.

### 6.3 Lector (instantánea)

`flock` compartido de `.readers.lock`; lista directorios, calcula las partes activas (ignora `tmp_*`, `delete_tmp_*`, `detached/`), abre todos los archivos que va a leer; suelta el lock. Los descriptores abiertos mantienen vivos los datos aunque la limpieza borre después. Si una apertura da `ENOENT`, repite la instantánea. Ningún lector bloquea a una fusión salvo durante la limpieza, ni la fusión a un lector salvo durante la instantánea.

### 6.4 Integridad

- Al abrir cada parte activa: verifica `checksums.txt` (línea `self`), que existan todos los archivos con su tamaño, y el CRC32C completo de `part.txt`, `primary.idx`, todos los `c<N>.mrk`, `stats.bin` y `sample.bin`. Si falla, la parte se renombra a `detached/broken_<nombre>`, se escribe en stderr `veta: detached broken part <nombre>: <motivo>` y se excluye.
- Al leer un bloque de `c<N>.bin`: si falla cualquier verificación de §5.3, la consulta aborta con exit 3 y `veta: corrupt part <nombre>: <motivo>` en stderr, y el proceso separa la parte a `detached/` antes de salir. Nunca se devuelven filas de un bloque que no verificó.
- `veta check` verifica todos los bloques de todas las partes y separa las corruptas (exit 3 si encontró alguna).

### 6.5 Recuperación

`veta recover`, y también `load`, `merge` y `merger` al empezar, hacen: (a) borrar cada `tmp_*` cuyo `owner.lock` se pueda tomar con `try_lock` (dueño muerto; `flock` se libera con `kill -9`) y cada `delete_tmp_*`; (b) borrar las partes cubiertas, bajo `.readers.lock` exclusivo; (c) verificar la integridad de apertura de §6.4. Los lectores nunca borran. Después de recuperar no queda ningún `tmp_*` y el conjunto de partes activas es el de antes del corte o el de después de la operación interrumpida, nunca una mezcla.

## 7. Política de fusión

- Ventanas candidatas: secuencias contiguas (por `min_block`) de 2 a 10 partes activas cuyo total de filas ≤ `--max-merge-rows` (por omisión 20 000 000) y cuya memoria estimada `filas × (bytes de la clave + 2)` ≤ `--merge-memory-mb` (por omisión 2048).
- Puntaje = suma de filas / filas de la parte mayor; se exige ≥ 1.5; gana el mayor puntaje y, a igualdad, el menor total de filas.
- `veta merge --final` ignora el umbral de puntaje y `--max-merge-rows`, y repite hasta que quede una sola parte activa.
- `veta merger` (proceso en segundo plano) repite selección y fusión; si no hay candidata duerme `--interval-ms` (por omisión 200).
- Algoritmo vertical: se cargan solo las columnas de la clave de las partes fuente, se calcula el orden de mezcla k-way y un vector con el índice de parte fuente por fila de salida (`u8`); después cada columna se escribe en flujo leyendo cada fuente secuencialmente gránulo a gránulo. Las estadísticas y la muestra de la parte nueva se recalculan de sus filas.

## 8. Códecs

### 8.1 Codificaciones de columna (por bloque)

El codificador prueba las candidatas aplicables y guarda la de menor tamaño; el byte `encoding` del bloque dice cuál:

| id | Nombre | Aplica a | Formato |
|---|---|---|---|
| 0 | `Plain` | todos | enteros/fechas/decimales/flotantes en LE de ancho fijo; booleanos como bits; cadenas: longitudes con `ForBitpack` + bytes concatenados |
| 1 | `ForBitpack` | enteros, fechas, timestamps, decimales | `min` (8 o 16 bytes) + `bit_width u8` + valores `v − min` empaquetados LSB-first en palabras de 64 bits, en mini-bloques de 128 valores; el último mini-bloque puede ser parcial |
| 2 | `DeltaBitpack` | ídem | primer valor + `ForBitpack` de las diferencias consecutivas |
| 3 | `Dict` | enteros y cadenas | diccionario (en orden de primera aparición, codificado con `Plain` o `ForBitpack`) + códigos con `ForBitpack` |
| 4 | `Rle` | enteros y cadenas | valores de cada racha (`Plain`/`ForBitpack`) + longitudes de racha con `ForBitpack` |

`decode(encode(x)) == x` para todo bloque, incluidos los NULL (los valores bajo NULL se normalizan a cero/vacío antes de codificar).

### 8.2 Compresor de propósito general `vlz`

Formato propio LZ77 orientado a velocidad de descompresión: `u32` longitud descomprimida; secuencias `token u8` (nibble alto: literales, 15 = extendido; nibble bajo: longitud de coincidencia − 4, 15 = extendido), bytes de extensión de 255 en 255, literales, desplazamiento de 3 bytes LE (1..2²⁴−1); la última secuencia solo lleva literales. Compresor con cadenas de hash (profundidad 16) y coincidencia perezosa de un paso. Se aplica a un payload si lo reduce al menos 5 %. `vlz` es un formato propio: no es LZ4 aunque se compare con él.

### 8.3 Snappy

Implementación propia de `format_description.txt` (Snappy 1.3.1): preámbulo varint con la longitud, literales y copias de 1, 2 y 4 bytes de desplazamiento. Compresor voraz con tabla hash. Se usa solo para Parquet.

## 9. Parquet

Implementación propia de Thrift compacto (`thrift-compact-protocol.md`, Thrift 0.25.0) y del formato (`parquet.thrift`, parquet-format 2.14.0). Los campos desconocidos se saltan.

**Lectura.** Esquemas planos: columnas REQUIRED u OPTIONAL; REPEATED o grupos anidados → error `nested types unsupported`. Páginas DATA_PAGE (v1), DATA_PAGE_V2 y DICTIONARY_PAGE; las demás se saltan. Si la cabecera trae `crc`, se verifica (CRC-32 IEEE). Codificaciones: PLAIN, PLAIN_DICTIONARY, RLE_DICTIONARY, RLE (niveles y booleanos), DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT; BIT_PACKED → error. Compresión: UNCOMPRESSED y SNAPPY; las demás → error `unsupported compression <nombre>`. Tipos: INT32/INT64 con anotaciones INT(8,16,32,64, con o sin signo; los sin signo se ensanchan) → enteros; DATE → `Date`; DECIMAL sobre INT32, INT64 o FIXED_LEN_BYTE_ARRAY (p ≤ 38) → `Decimal`; TIMESTAMP(MILLIS|MICROS|NANOS) → `Timestamp` en segundos (piso); BYTE_ARRAY sin anotación, STRING, UTF8 o ENUM → `Varchar`; FLOAT/DOUBLE → `Float64`; BOOLEAN → `Boolean`; INT96 → error. Estadísticas: `min_value`/`max_value` (campos 5 y 6) y, si faltan, `min`/`max` (1 y 2) solo para enteros con signo y fechas; `null_count`.

**Escritura** (`veta export`). Un archivo con grupos de `--row-group-rows` filas (por omisión 1 000 000) y páginas de hasta 8192 filas, versión de página `--page-version 1|2` (por omisión 1). `--encoding`: `plain`; `dict` (RLE_DICTIONARY con página de diccionario PLAIN; si el diccionario pasa de 65 536 entradas en un chunk, ese chunk cae a PLAIN); `delta` (enteros, fechas y decimales INT64 → DELTA_BINARY_PACKED; cadenas → DELTA_LENGTH_BYTE_ARRAY; el resto PLAIN); `delta-byte-array` (igual que `delta` pero cadenas → DELTA_BYTE_ARRAY). `--compression none|snappy` (por omisión snappy). Columnas `nullable` → OPTIONAL con niveles de definición RLE; las demás REQUIRED. Tipos: `Int16` → INT32 + INT(16,true); `Int32` → INT32; `Int64` → INT64; `Decimal(p≤18)` → INT64 + DECIMAL; `Date` → INT32 + DATE; `Timestamp` → INT64 + TIMESTAMP(MILLIS, isAdjustedToUTC=false); `Varchar` → BYTE_ARRAY + STRING; `Float64` → DOUBLE; `Boolean` → BOOLEAN; `Int128` y `Decimal(p>18)` → error. Cada column chunk lleva `Statistics` con `min_value`, `max_value` (exactos, sin truncar) y `null_count`. `created_by = "veta <versión>"`.

**Consulta directa.** `read_parquet('<ruta>')` es una tabla en `FROM`. Los filtros sobre columnas con estadísticas se usan para saltar grupos de filas (§11.4).

## 10. SQL

### 10.1 Sentencias

- `CREATE TABLE nombre (col tipo [NOT NULL | NULL], …, [PRIMARY KEY (cols)]) [ENGINE = ident] [ORDER BY (cols)] [SETTINGS k = v, …]`. `ORDER BY` equivale a `PRIMARY KEY`; `ENGINE` y `SETTINGS` se aceptan y se ignoran. Sin `NOT NULL` la columna es nullable. Así se aceptan sin cambios `create.sql` de ClickBench para ClickHouse y `bench/tpch/create.sql`.
- `SELECT …` (con `WITH`).
- `EXPLAIN [ANALYZE] SELECT …` (equivale a `--explain-analyze` cuando lleva ANALYZE).
- Comentarios `-- …` y `/* … */`. Identificadores sin distinguir mayúsculas; las columnas de salida conservan el texto escrito.

### 10.2 Consultas

```
query   := [WITH name AS (query) {, name AS (query)}] select
select  := SELECT [DISTINCT] item {, item} [FROM from {, from}] [WHERE e] [GROUP BY g {, g}]
           [HAVING e] [ORDER BY e [ASC|DESC] [NULLS FIRST|LAST] {, …}] [LIMIT n] [OFFSET n]
item    := * | e [[AS] alias]
from    := source {join}
source  := tabla [[AS] alias] | (query) [AS] alias [(col {, col})] | read_parquet('ruta') [[AS] alias]
join    := [INNER] JOIN source ON e | LEFT [OUTER] JOIN source ON e | CROSS JOIN source
g       := e | ordinal | alias de la lista SELECT
```

Los alias de la lista SELECT se pueden usar en GROUP BY, HAVING y ORDER BY; un entero en GROUP BY u ORDER BY es posicional.

### 10.3 Expresiones y funciones

Literales enteros, decimales, cadenas `'…'` (con `''`), `NULL`, `TRUE`, `FALSE`, `DATE '…'`, `TIMESTAMP '…'`; columnas calificadas o no; `+ − * / %`, `−` unario; `= <> != < <= > >=`; `AND OR NOT`; `IS [NOT] NULL`; `[NOT] BETWEEN`; `[NOT] IN (lista)`; `[NOT] IN (subconsulta)`; `[NOT] EXISTS (subconsulta)`; subconsulta escalar; `[NOT] LIKE` (`%`, `_`, escape `\`); `CASE WHEN … THEN … [ELSE …] END`; `CAST(e AS tipo)`.

Funciones escalares: `extract(year|month|day|hour|minute|second FROM e)`, `length(s)` y `STRLEN(s)`, `substring(s FROM a [FOR b])` y `substring(s, a[, b])`, `REGEXP_REPLACE(s, patrón, reemplazo)`, `DATE_TRUNC('minute'|'hour'|'day'|'month'|'year', e)`. Agregados: `COUNT(*)`, `COUNT([DISTINCT] e)`, `SUM`, `AVG`, `MIN`, `MAX`.

Regex propia (máquina de Pike con capturas, tiempo lineal): literales, `\.` y escapes de metacaracteres, `\d \w \s`, `.`, clases `[...]` con rangos y negación, `^ $`, grupos `( )` y `(?: )`, `|`, `* + ? {n} {n,} {n,m}` voraces. Cualquier otra sintaxis → error al compilar el patrón.

Este es exactamente el subconjunto que exigen las 43 consultas de ClickBench (dialecto ClickHouse, que es el que acepta veta) y las 22 de TPC-H (texto de DuckDB v2.0.0); lo que no está aquí da error 1 `unsupported: …`.

### 10.4 Subconsultas

El binder las desanida siempre (no es una regla del optimizador): `EXISTS`/`NOT EXISTS` correlacionados por igualdades → semi/anti join con las igualdades como claves y el resto de predicados correlacionados como residuales; `IN (subconsulta)` → semi join; `NOT IN (subconsulta)` → anti join consciente de NULL (si la subconsulta produce algún NULL, ninguna fila pasa); subconsulta escalar sin correlación → se evalúa una vez (error si da más de una fila); subconsulta escalar correlacionada con agregado → agregación agrupada por las columnas de correlación + join `single` por esas columnas (un `COUNT` sin coincidencias vale 0). Correlaciones de otra forma → error 1.

## 11. Planificación, optimizador y estadísticas

### 11.1 Plan

Nodos lógicos: `Scan`, `ParquetScan`, `Filter`, `Project`, `Aggregate`, `Join { kind: Inner|Left|Semi|Anti|NullAwareAnti|Single|Cross, keys, residual }`, `Sort`, `TopN`, `Limit`. Los predicados de igualdad entre dos relaciones se vuelven claves de join al construir el plan aunque el optimizador esté apagado.

### 11.2 Reglas (con el optimizador encendido, en este orden)

1. Plegado de constantes y simplificación booleana.
2. Empuje de predicados: se parten en conjunciones y bajan a través de `Project` y de joins internos a ambos lados; en un `Left` solo bajan al lado preservado (izquierdo); nada baja desde arriba hacia el lado que aporta NULL.
3. Grafo de joins: cada árbol maximal de joins `Inner`/`Cross` se aplana en relaciones + predicados; enumeración **DPccp** (árboles bushy sin productos cruzados) hasta 12 relaciones, voraz (GOO) por encima; costo `C = Σ_joins (2·|build| + |probe| + |out|)`; en cada hash join el lado de menor cardinalidad estimada construye. Los joins `Left`, `Semi`, `Anti`, `NullAwareAnti` y `Single` no se reordenan ni cambian de lado.
4. Derivación de filtros de escaneo: conjunciones de una sola tabla con forma `col op literal`, `BETWEEN`, `IN (literales)` → `ScanFilter` (rangos e igualdades por columna) para podar partes, gránulos y grupos de filas; el predicado completo se sigue evaluando sobre las filas.
5. Poda de columnas (también activa con el optimizador apagado).
6. `Sort` + `Limit` → `TopN`.

### 11.3 Modos

- `--no-optimizer`: no aplica 1, 2, 3, 4 ni 6. Los joins se arman en **orden sintáctico conectado**: izquierda a derecha en el orden de `FROM`, tomando en cada paso la siguiente relación en ese orden que tenga un predicado de join con lo ya unido (producto cruzado solo si ninguna lo tiene); el lado derecho construye; los demás predicados quedan en un `Filter` encima del árbol de joins; no hay poda.
- `--join-order syntactic`: solo cambia la regla 3 por el orden sintáctico conectado; todo lo demás sigue encendido.
- `--no-prune`: solo apaga la poda de partes, gránulos y grupos de filas.

### 11.4 Poda

Para cada parte, rango de claves por gránulo `g` = `[idx[g], idx[g+1]]` (incluye `idx[g+1]` porque la primera fila del gránulo siguiente acota el último valor de `g`). Análisis por intervalos sobre la tupla de la clave: la columna 0 toma `[lo0, hi0]`; la columna `k` solo se acota si las columnas `0..k−1` son iguales en ambos extremos. Un gránulo se lee si el `ScanFilter` puede ser verdadero en su rango. Una parte se descarta si su rango completo o el mín/máx de `stats.bin` de alguna columna filtrada excluyen el filtro. En Parquet, un grupo de filas se salta si las estadísticas de alguna columna filtrada lo excluyen.

### 11.5 Estimación de cardinalidad

- Filas de una tabla: suma exacta de las partes.
- Selectividad de los predicados de una tabla: se evalúan sobre la unión de las muestras de sus partes; `sel = coincidencias / filas de muestra`, y `0.5 / filas de muestra` si no coincide ninguna.
- NDV por columna: HLL fusionado de las partes, tope en filas; tras un filtro de selectividad `s` sobre `n` filas, `ndv' = ndv · (1 − (1 − s)^(n/ndv))`.
- Fracción de NULL `nf = nulls / rows`.
- Join interno por igualdad de una columna: `|L ⋈ R| = |L| · |R| · (1 − nf_L)(1 − nf_R) / max(ndv'_L, ndv'_R)`.
- Claves compuestas: si las columnas de un lado forman un prefijo de su clave primaria, su NDV combinado sale del HLL de ese prefijo; el lado sin NDV conocido toma `min(filas, Π ndv_i, NDV del otro lado)` (contención); si ningún lado se conoce, producto de selectividades con retroceso exponencial (`s1 · s2^½ · s3^¼ …`, ordenadas de menor a mayor).
- `GROUP BY`: `min(filas de entrada, Π ndv')`.

### 11.6 EXPLAIN ANALYZE

`--explain-analyze` ejecuta la consulta y en lugar de filas imprime en stdout un solo objeto JSON (el árbol físico):

```json
{"op": "HashJoin", "kind": "inner", "est_rows": 1234.5, "actual_rows": 1200,
 "relations": [{"alias": "o", "table": "orders"}, {"alias": "l", "table": "lineitem"}],
 "predicates": ["o.o_orderkey = l.l_orderkey", "o.o_orderdate < CAST('1995-03-15' AS DATE)"],
 "children": [ … ]}
```

`op` ∈ `Scan, ParquetScan, Filter, Project, HashAggregate, HashJoin, NestedLoopJoin, Sort, TopN, Limit`; `kind` en joins ∈ `inner, left, semi, anti, null_aware_anti, single, cross`. Todo nodo lleva `relations` (relaciones base de su subárbol, con el alias de la consulta o el nombre de la tabla) y `predicates` (todos los predicados aplicados en el subárbol, en SQL válido para DuckDB y para veta: columnas `alias.col`, fechas `CAST('…' AS DATE)`, timestamps `CAST('…' AS TIMESTAMP)`, cadenas con `''`, todo con paréntesis explícitos).

## 12. Ejecución

- Vectores de hasta 8192 filas alineados con los gránulos; `Chunk { columns: Vec<Vector>, len }`; un `Vector` es un buffer tipado más un mapa de validez opcional; los filtros producen vectores de selección; las cadenas son `offsets u32 + bytes`.
- Morsel = hasta 16 gránulos contiguos de una parte (o un grupo de filas de Parquet). Un despachador con contador atómico reparte morsels a `--threads` hilos de trabajo (por omisión `available_parallelism`). Los pipelines terminan en puntos de ruptura: agregación hash (tablas por hilo, después 64 particiones radix fusionadas en paralelo), construcción de hash join (paralela y particionada; el sondeo es otro pipeline), orden (corridas ordenadas por hilo + mezcla k-way), top-N (montículo por hilo + fusión) y límite (bandera de parada temprana).
- Núcleos por columna sobre slices sin despacho dinámico por fila; tablas hash especializadas para claves de ancho fijo (empaquetadas en `u64`/`u128`) y para cadenas (arena + hash); `COUNT(DISTINCT)` con conjuntos hash particionados.
- Contador de memoria global (tablas hash, buffers materializados); `--memory-limit-mb` (por omisión 10240); si se excede, exit 4. No hay derrame a disco.
- Hash interno: el que el crate elija (no se persiste). Hash persistido en HLL: xxh64 propio, semilla 0, sobre la representación canónica del valor (`i64`/`i128` LE, bytes de la cadena).

## 13. CLI `veta`

`target/release/veta <subcomando> …`. Códigos de salida: 0 bien; 1 error del usuario (sintaxis, binding, `unsupported`); 2 E/S o interno; 3 parte corrupta detectada; 4 límite de memoria.

| Subcomando | Contrato |
|---|---|
| `sql --db D [opciones] "<sentencia>"` | Ejecuta una sentencia. Opciones: `--threads N`, `--format tsv` (sin cabecera, por omisión) o `tsv-header`, `--no-optimizer`, `--join-order cost|syntactic`, `--no-prune`, `--stats`, `--explain-analyze`, `--memory-limit-mb N` |
| `load --db D --table T (--parquet F… \| --tsv F…) [--batch-rows N]` | Cada archivo es un lote (o se parte en lotes de N filas); cada lote, una parte atómica. Columnas de Parquet por nombre (sin mayúsculas); conversiones: entero → entero (con rango), → `Date` (días), → `Timestamp` (segundos); BYTE_ARRAY → `Varchar`; decimales exactos de igual escala; NULL en columna NOT NULL → error 1. TSV: columnas en orden de tabla, formato de §13.1 |
| `merge --db D --table T [--final] [--max-merge-rows N] [--merge-memory-mb N]` | Fusiones en primer plano (§7) |
| `merger --db D [--interval-ms N] [--max-merge-rows N] [--merge-memory-mb N]` | Fusionador en segundo plano para todas las tablas, hasta que lo maten |
| `recover --db D [--table T]` | §6.5 |
| `check --db D --table T` | §6.4 |
| `parts --db D --table T` | TSV: `nombre, filas, nivel, bytes` de cada parte activa |
| `export --db D --sql "<SELECT>" --out F [--encoding plain|dict|delta|delta-byte-array] [--compression none|snappy] [--page-version 1|2] [--row-group-rows N]` | §9 |
| `parquet-meta --file F` | TSV por column chunk: `row_group, columna, tipo físico, codificaciones, compresión, min, max, null_count` (valores con el formato de §13.1) |
| `codec-roundtrip [--db D --table T] [--random N --seed S]` | Para cada columna de la tabla y/o N columnas aleatorias de todos los tipos (longitudes, NULL y distribuciones variadas, incluidas longitudes no múltiplo de 128 y de 8192), comprueba `decode(encode(x)) == x` para cada codificación aplicable y para vlz y Snappy; imprime `OK`/`FAIL <códec> <columna>` por caso y `codec-roundtrip: <n> cases, <f> failures`; exit 0 si no hay fallos |
| `dump-columns --db D --table T --out DIR` | Escribe `DIR/<columna>.raw` con los valores en `Plain` sin NULL (cadenas: `u32` LE de longitud + bytes) |
| `codec-bench --compressor vlz --file F… [--iterations N]` | Un objeto JSON por línea y archivo: `{"file", "raw_bytes", "compressed_bytes", "compress_mb_s", "decompress_mb_s"}`; un hilo, en memoria, mejor de N ≥ 3 iteraciones y ≥ 1 s |

`--stats` escribe en stderr una línea `VETA_STATS {"parts_total":…,"parts_read":…,"granules_total":…,"granules_read":…,"rows_read":…,"row_groups_total":…,"row_groups_read":…}` (todas las claves siempre, 0 si no aplica).

### 13.1 Formato de valores (TSV)

Como `TSV` de ClickHouse: columnas separadas por `\t`, filas por `\n`; NULL = `\N`; en cadenas se escapan `\\`, `\t`, `\n`, `\r`, `\0`, `\b`, `\f`; enteros en decimal; `Float64` en la representación más corta que se relee igual (`inf`, `-inf`, `nan`); decimales con exactamente `scale` dígitos; `Date` = `YYYY-MM-DD`; `Timestamp` = `YYYY-MM-DD HH:MM:SS`; booleanos `true`/`false`.

## 14. Arnés

### 14.1 Forma

Paquete `harness/` (solo stdlib), invocado como `python3 -m harness <comando> [opciones]`. Módulos: `__main__.py` (argparse), `config.py` (rutas y fijaciones del apéndice A), `fetch.py`, `engines.py` (adaptadores veta/DuckDB/ClickHouse), `normalize.py`, `compare.py`, `sqltext.py` (partición de cláusulas de nivel superior), `clickbench.py`, `tpch.py`, `gen.py` (datos y consultas aleatorias), `oracles.py`, `durability.py`, `parquet_interop.py`, `perf.py`, `mutants.py`, `meta.py` (tamaño y dependencias), `report.py`. Tests en `harness/tests/test_*.py` con datos sintéticos, sin binarios externos ni red.

Rutas: `VETA_DATA` (por omisión `<repo>/.veta-data`, ignorado por git) y `VETA_BIN` (por omisión `<repo>/target/release/veta`). Nada se escribe en `/tmp`: los temporales van en `$VETA_DATA/scratch/`.

```
$VETA_DATA/bin/duckdb  bin/clickhouse  duckdb-ext/
$VETA_DATA/clickbench/hits_{0..9}.parquet  queries-clickhouse.sql  queries-duckdb.sql  create-clickhouse.sql  create-duckdb.sql
$VETA_DATA/tpch/queries/q01.sql … q22.sql   tpch/sf1/*.parquet  tpch/sf10/*.parquet  tpch/sf1.duckdb  tpch/sf10.duckdb
$VETA_DATA/parquet-testing/delta_byte_array.parquet  delta_encoding_optional_column.parquet
$VETA_DATA/db/veta-clickbench/  db/veta-tpch-sf1/  db/veta-tpch-sf10/  db/duckdb-clickbench.duckdb  db/clickhouse/
$VETA_DATA/runs/<comando>/   $VETA_DATA/scratch/
```

### 14.2 Contrato común de los comandos

- Imprimen progreso y terminan con **una** línea `RESULT <nombre> <PASS|FAIL|SKIP> <resumen>`.
- Salida: 0 PASS; 1 FAIL; 77 SKIP porque falta `$VETA_DATA/bin/duckdb`, `$VETA_DATA/bin/clickhouse` o `lz4` (solo `fetch` descarga; ningún otro comando descarga); 2 error de uso o de infraestructura (falta el binario de veta, faltan datos sin cargar).
- Escriben `$VETA_DATA/runs/<nombre>/report.json` = `{"command", "status", "summary", "items": [...]}` y las salidas crudas de cada motor en `$VETA_DATA/runs/<nombre>/raw/`.

### 14.3 Carga de datos

- `fetch`: descarga y verifica todo el apéndice A (idempotente: no vuelve a bajar lo que ya verifica).
- `load clickbench [--rebuild-veta]`: veta: `sql` con `create-clickhouse.sql` y `load --parquet hits_0..9` (10 partes); DuckDB: `create-duckdb.sql` + `INSERT INTO hits SELECT * REPLACE (make_date(EventDate) AS EventDate, epoch_ms(EventTime * 1000) AS EventTime, epoch_ms(ClientEventTime * 1000) AS ClientEventTime, epoch_ms(LocalEventTime * 1000) AS LocalEventTime) FROM read_parquet([...], binary_as_string=True)`; ClickHouse: `create-clickhouse.sql`, `INSERT INTO hits SELECT * FROM file('<ruta>/hits_{0..9}.parquet', Parquet)`, `OPTIMIZE TABLE hits FINAL`. `--rebuild-veta` rehace solo la base de veta.
- `load tpch --sf 1|10 [--rebuild-veta]`: DuckDB genera en `tpch/sf<N>.duckdb` con `SET memory_limit='4GB'; LOAD tpch; CALL dbgen(sf=N)` y exporta cada tabla con `COPY t TO 'tpch/sf<N>/t.parquet' (FORMAT parquet)`; veta: `bench/tpch/create.sql` y `load --parquet … --batch-rows 1000000`; ClickHouse: base `tpch_sf<N>`, `bench/tpch/create.sql` con ` ENGINE = MergeTree` añadido a cada tabla e `INSERT … SELECT * FROM file(…, Parquet)`.
- `bench/tpch/create.sql`: las 8 tablas con los tipos de `schema.sql` de DuckDB y estas claves primarias: lineitem `(l_orderkey, l_linenumber)`, orders `(o_orderkey)`, customer `(c_custkey)`, part `(p_partkey)`, partsupp `(ps_partkey, ps_suppkey)`, supplier `(s_suppkey)`, nation `(n_nationkey)`, region `(r_regionkey)`.

### 14.4 Invocación de los motores

- veta: `$VETA_BIN sql --db <base> --format tsv [--stats] "<sql>"`; tiempo = reloj de pared del proceso medido por el arnés.
- DuckDB: entorno con `DUCKDB_AGENT_MODE=0`; `bin/duckdb -no-agent -csv -noheader -nullvalue '\N' <archivo.duckdb> -c "SET extension_directories=['<VETA_DATA>/duckdb-ext']" -c ".timer on" -c "<sql>"`; tiempo = `Run Time (s): real X` (se quita de las filas).
- ClickHouse: `bin/clickhouse local --path <VETA_DATA>/db/clickhouse --session_timezone UTC --join_use_nulls 1 [--database tpch_sf<N>] --format TSV --time -q "<sql>"`; tiempo = última línea de stderr.
- Las salidas se leen como bytes (`errors="surrogateescape"`) y las filas se parten solo por `\n`.

## 15. Comparación de resultados

### 15.1 Normalización de celdas (igual para los tres motores)

`\N` → NULL; `true/false` → 1/0; entero `^-?\d+$` → entero; otro número (incluye exponente, `inf`, `nan`) → flotante; `YYYY-MM-DD HH:MM:SS(.f*)?` → la misma cadena sin fracción nula; lo demás, bytes de la cadena (TSV: se deshacen los escapes de §13.1 y además `\'`; CSV de DuckDB: módulo `csv`). Dos números son iguales si ambos son enteros e idénticos, o si `|a − b| ≤ 1e-9 · max(|a|, |b|) + 1e-12`.

### 15.2 Reglas por consulta (decididas por el texto: `ORDER BY` y `LIMIT/OFFSET` de nivel superior)

- **multiconjunto**: sin `ORDER BY` ni `LIMIT` → multiconjuntos iguales.
- **ordenada**: con `ORDER BY` → se calcula la clave de orden de cada fila; las secuencias de claves deben ser idénticas y, dentro de cada grupo de filas con la misma clave, los multiconjuntos iguales.
- **borde de LIMIT**: con `ORDER BY` y `LIMIT` → como *ordenada*, salvo que en los grupos de la primera y la última clave de la ventana basta con que coincidan las claves (empates válidos).
- **LIMIT sin orden** (solo ClickBench Q18): mismo número de filas y cada fila del motor existe, con sus mismos valores, en el resultado de la referencia sin `LIMIT`, comprobado en la referencia con `SELECT c1, … FROM (<consulta sin LIMIT con alias c1…cn>) AS t WHERE (c1 = v1 AND …) OR …`.

Si una clave de `ORDER BY` no está en la lista SELECT (por alias, ordinal o texto idéntico), el arnés compara la variante **aumentada** que añade esas expresiones al final de la lista SELECT en los dos motores, y además verifica que la salida de la consulta original tenga el mismo número de filas.

### 15.3 Falsificación

`--tamper Q` cambia el valor de la primera celda de la primera fila del resultado de referencia de la consulta `Q` (enteros +1; flotantes ×1.5 + 1; cadenas `+ "x"`; NULL → 0) antes de comparar; el comando debe terminar con 1.

### 15.4 Reescrituras por discrepancia entre oráculos (medidas en `vault/RESEARCH.md` §6)

- ClickBench Q4 en ClickHouse: `SELECT AVG(toInt128(UserID)) FROM hits;` (su suma en Int64 se desborda).
- ClickBench Q29 en DuckDB: el patrón empieza con `(?s)` (en DuckDB `.` no coincide con `\n`; en ClickHouse y veta sí).
- ClickHouse siempre con `--join_use_nulls 1` (TPC-H Q13) y `--session_timezone UTC`.

Ninguna otra reescritura está permitida; veta siempre corre el texto sin cambios.

## 16. Umbrales

| Medida | Umbral |
|---|---|
| Tolerancia de coma flotante | relativa 1e-9 + absoluta 1e-12 |
| Tiempo por consulta, ClickBench (10 M filas) y TPC-H SF10 | `r = (t_veta + 0.01) / (t_ref + 0.01)` con el mejor de 3; cada consulta `r ≤ 15` y media geométrica `≤ 4`, contra ClickHouse y contra DuckDB por separado |
| RSS máximo de un proceso veta (`/usr/bin/time -f %M`) | ≤ 10 GiB (10 485 760 KB) en cada una de las 43 + 22 (SF10) |
| Aceleración multihilo | `T(--threads 1) / T(--threads 12) ≥ 3.0` sobre la suma de ClickBench |
| Reordenación de joins (TPC-H SF1) | `T(syntactic) / T(cost) ≥ 1.5` sobre la suma de las 22, con resultados iguales |
| Error q de cardinalidad, TPC-H SF1 | `q = max(e/a, a/e)` con `e, a ≥ 1`; mediana ≤ 2 y percentil 90 ≤ 10 |
| Error q, suite de NULL | máximo ≤ 3 |
| Bytes en disco | `du -sb` de la tabla `hits` de veta (una parte) ≤ 1.10 × `sum(bytes_on_disk)` de ClickHouse |
| Compresor vlz frente a `lz4 -b1` | ratio agregado ≥ 1.0 × el de LZ4 y velocidad agregada de descompresión ≥ 0.33 × la de LZ4 |
| Consultas aleatorias | TLP ≥ 1000; optimizador encendido/apagado ≥ 1000 + las 65 |
| Iteraciones | `kill -9` en fusión ≥ 100; en inserción ≥ 100; lector concurrente ≥ 200 |

Velocidad agregada = `Σ bytes crudos / Σ (bytes_i / velocidad_i)`. Ratio agregado = `Σ crudos / Σ comprimidos`.

## 17. Comandos de verificación

Todos siguen §14.2. `<N>/<M> equal` es el formato de resumen de las comparaciones.

| Comando | Criterio de la idea | PASS si |
|---|---|---|
| `clickbench-verify --against duckdb\|clickhouse [--tamper Q]` | ClickBench contra cada oráculo | resumen `43/43 equal`; con `--tamper` debe dar FAIL |
| `tpch-verify --sf 1` | contra `tpch_answers()` | `22/22 equal` |
| `tpch-verify --sf 10` | contra DuckDB en SF10 | `22/22 equal` |
| `perf clickbench\|tpch` | rendimiento (§16) | CSV `runs/perf-<b>/times.csv` (`query,engine,run1,run2,run3,best`) y umbrales cumplidos; 77 si falta un oráculo |
| `rss` | tope de memoria | las 65 bajo el tope |
| `threads` | aceleración multihilo | factor ≥ 3.0 |
| `tlp [--queries N] [--seed S]` | oráculo TLP | 0 discrepancias y 0 errores en ≥ 1000 consultas |
| `optimizer-oracle [--queries N] [--seed S]` | encendido contra apagado | todas iguales (aleatorias + 65) |
| `join-reorder` | reordenación útil | factor ≥ 1.5 y resultados iguales |
| `cardinality` | error q | umbrales de §16; conteos de veta (`--no-optimizer`) = conteos de DuckDB |
| `pruning` | poda por índice | en Q37–Q43 de ClickBench, `granules_read` con poda < sin poda (`--no-prune`) y mismos resultados |
| `codec-roundtrip` | propiedades de los códecs | `veta codec-roundtrip --random 2000 --seed 1` y sobre la tabla `hits` terminan con 0 |
| `disk-bytes` | bytes en disco | §16 (antes hace `veta merge --final` en la base de ClickBench) |
| `compressor` | vlz contra LZ4 | §16 sobre `dump-columns` de `hits` |
| `merge-equivalence` | fusionar no cambia resultados | `hits` cargada en lotes de 100 000 filas (100 partes), 43 resultados antes y después de `merge --final`, iguales, y una sola parte activa |
| `concurrent-reader [--iterations N]` | lector durante fusiones | `hits_0` en lotes de 20 000 filas, `merger` corriendo, N iteraciones de 5 consultas fijas (Q2, Q8, Q16, Q28, Q31) sin diferencias ni errores, y el número de partes activas bajó durante el bucle |
| `crash-merge [--iterations N]` | `kill -9` en fusión | por iteración: copia prístina de `hits_0` en 20 lotes, `merge --final` muerto con SIGKILL en un instante uniforme en `[0, T]` (T = duración medida de una fusión completa), `recover`; sin `tmp_*`, `check` con 0, las 5 consultas iguales a la referencia; ≥ 80 % de los cortes con el proceso todavía vivo |
| `crash-insert [--iterations N]` | `kill -9` en inserción | tabla `(batch_id BIGINT, i BIGINT, payload VARCHAR, PRIMARY KEY (batch_id, i))`, un `load --tsv` de 200 000 filas por iteración muerto en `[0, T]`; tras `recover`, cada `batch_id` presente tiene exactamente 200 000 filas, sin `tmp_*`, `check` con 0; ≥ 80 % de cortes con el proceso vivo |
| `corruption` | detección de partes corruptas | para `part.txt`, `primary.idx`, `c<N>.bin`, `c<N>.mrk`, `stats.bin`, `sample.bin`, `checksums.txt`: sobre una copia de una tabla de 4 partes se cambia un byte; una consulta que lee esa columna termina con 3 o devuelve exactamente el resultado sin esa parte; la siguiente devuelve el resultado sin esa parte; la parte está en `detached/` |
| `parquet-read-duckdb` | DuckDB escribe, veta lee | por cada configuración (v1 por omisión, v1 sin diccionario, v2, v2 sin diccionario; con `COMPRESSION uncompressed` y `snappy`; columnas de `hits` y de `lineitem` SF1, con columnas nullable) más los dos archivos de parquet-testing (DELTA_BYTE_ARRAY): `parquet_metadata` confirma las codificaciones y `SELECT *` de veta = de DuckDB; cubiertas PLAIN, PLAIN_DICTIONARY, RLE_DICTIONARY, RLE, DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY y BYTE_STREAM_SPLIT |
| `parquet-read-hits` | lee los `hits_N` publicados | por archivo, `count(*)`, `sum` de cada columna numérica y `sum(length(s))` de cada cadena = DuckDB |
| `parquet-write-duckdb` | veta escribe, DuckDB lee | `hits` (`dict`+`snappy` y `delta`+`none`) y las 8 tablas SF1 (las 4 codificaciones × 2 compresiones): `EXCEPT ALL` en ambos sentidos contra la tabla de DuckDB = 0 filas |
| `parquet-write-clickhouse` | ClickHouse lee lo de veta | los mismos archivos con `file(…, Parquet)` y `EXCEPT ALL` contra la tabla de ClickHouse = 0 filas |
| `parquet-stats` | estadísticas del pie | para cada chunk de esos archivos, `parquet-meta` de veta = `stats_min_value/stats_max_value/stats_null_count` de `parquet_metadata()` = mín/máx/NULL que DuckDB calcula sobre las filas de ese grupo (`file_row_number`) |
| `parquet-rowgroup-skip` | salto de grupos de filas | `lineitem` SF1 exportado ordenado por `l_shipdate` con grupos de 100 000 filas; 5 consultas con filtro selectivo sobre `l_shipdate` vía `read_parquet`: `row_groups_read < row_groups_total` y mismo resultado que con `--no-prune` |
| `mutants` | catálogo de mutantes (§18) | cada mutante hace fallar al menos un comando y se lista cuál |
| `size` | arnés menor que el motor | imprime `engine_lines=<n> harness_lines=<m>` (líneas no vacías de `crates/**/*.rs` y `harness/**/*.py`); PASS si m < n |
| `deps` | dependencias permitidas | `Cargo.lock` solo con paquetes `veta-*` sin `source`; ningún `[dependencies]` externo; el arnés solo importa stdlib y `harness` |
| `all` | verificación total | corre todos los anteriores en este orden; PASS si cada uno es PASS o SKIP (77) |

La suite de NULL de `cardinality`: tablas generadas `na(k BIGINT, v BIGINT)` con 200 000 filas y 90 % de `k` NULL, `nb(k BIGINT, w BIGINT)` con 20 000 filas y `k` único no nulo; 10 consultas `SELECT count(*) FROM na JOIN nb ON na.k = nb.k [WHERE …]` con filtros variados.

TLP y el oráculo de optimizador usan el mismo generador (`gen.py`, semilla fija): 3 tablas con columnas `BIGINT`, `INTEGER`, `DECIMAL(10,2)`, `VARCHAR` (alfabeto corto), `DATE` y `BOOLEAN`, 0–2000 filas, 0–50 % de NULL por columna, cargadas por TSV en varios lotes; consultas con `WHERE`, joins internos y `LEFT JOIN` con predicados en `ON` y en `WHERE` sobre el lado que aporta NULL, `DISTINCT`, `GROUP BY` y agregados `COUNT/SUM/MIN/MAX` (sin `AVG`, sin `SUM` sobre `DOUBLE`), y `ORDER BY` total con `LIMIT`. TLP compara la consulta con la unión de sus particiones `p`, `NOT p` y `p IS NULL` (agregados recompuestos: COUNT y SUM suman, MIN/MAX de MIN/MAX).

## 18. Catálogo de mutantes

Cada mutante es una feature de Cargo, apagada por omisión, en el crate donde vive el código, reexportada por `veta-cli` como `mutant-<nombre>`. `mutants` compila cada uno con `cargo build --release -p veta-cli --features mutant-<nombre> --target-dir target/mutants/<nombre>`, corre con ese `VETA_BIN` los comandos `codec-roundtrip`, `pruning`, `clickbench-verify --against duckdb`, `tpch-verify --sf 1`, `tlp`, `optimizer-oracle` y `cardinality`, e imprime `MUTANT <nombre> KILLED by <comandos>` o `MUTANT <nombre> SURVIVED`.

| Feature | Crate | Falla plantada |
|---|---|---|
| `mutant-prune-extra-granule` | veta-storage | la poda descarta un gránulo de más (el último de cada rango seleccionado) |
| `mutant-prune-wrong-bound` | veta-storage | el rango de un gránulo usa `idx[g]` como cota superior en vez de `idx[g+1]` |
| `mutant-pushdown-outer` | veta-sql | el empuje de predicados baja filtros de `WHERE` al lado que aporta NULL de un `Left` |
| `mutant-reorder-outer` | veta-sql | el grafo de joins trata un `Left` como `Inner` y lo reordena |
| `mutant-estimate-ignore-null` | veta-sql | la estimación de joins omite los factores `(1 − nf)` |
| `mutant-codec-drop-last-block` | veta-codec | `ForBitpack` no escribe el último mini-bloque parcial |

## 19. Decisiones cerradas

Las 23 decisiones delegadas a K0 por la idea, en su orden, y después las que abrió el diseño.

1. **Lenguaje.**
   Decisión: motor en Rust 1.98.1 (edition 2024, sin crates externos); arnés en Python ≥ 3.12 con solo la biblioteca estándar.
   Razón: Rust da control de memoria y SIMD sin GC dentro de 16 GB y el compilador hace cumplir los límites entre crates; Python es parte del stack de la vacante y basta para orquestar procesos.
2. **Arquitectura y módulos.**
   Decisión: siete crates con el grafo de §2 y un binario CLI que corre una sentencia por proceso.
   Razón: Cargo vuelve verificables los límites entre paquetes, y un proceso por consulta hace reales el `kill -9`, la concurrencia entre lector y fusionador y la medición de RSS por consulta.
3. **Formato de partes, marcas, índice y sumas.**
   Decisión: §5 (gránulo fijo de 8192 filas, un bloque por gránulo y columna, marcas `u64`, índice con primera fila por gránulo más la última, CRC32C por bloque y `checksums.txt` con autoverificación).
   Razón: es la estructura de MergeTree reducida a lo que exigen los criterios, y cada archivo de una parte tiene una verificación propia para el criterio de corrupción.
4. **Publicación atómica y recuperación.**
   Decisión: §6: directorio `tmp_*` con `owner.lock`, `fsync` y `rename`; cobertura por rango de bloques; limpieza bajo `flock` exclusivo y lectores con `flock` compartido.
   Razón: `rename` es atómico en el mismo sistema de archivos y `flock` se libera solo al morir el proceso, así que la recuperación distingue sin ambigüedad un temporal huérfano de uno vivo.
5. **Códecs, compresor propio y compresiones de Parquet.**
   Decisión: §8; ninguna biblioteca de compresión; Parquet con UNCOMPRESSED y SNAPPY propios; ZSTD, GZIP, LZ4, LZ4_RAW y BROTLI fuera.
   Razón: los `hits_N.parquet` y la salida por omisión de DuckDB usan SNAPPY, que se implementa en pocas líneas; ClickHouse solo necesita leer lo que escribe veta.
6. **Política de fusión.**
   Decisión: §7 (ventanas contiguas de 2–10 partes, puntaje ≥ 1.5, ≤ 20 M filas y ≤ 2 GiB estimados; `--final` hasta una parte; fusión vertical).
   Razón: limita la amplificación de escritura sin bloquear y la memoria de la fusión queda acotada por las columnas de la clave, no por la tabla.
7. **Optimizador.**
   Decisión: §11 (reglas fijas, DPccp con costo `C_out` ajustado al hash join, estimación por muestras, HLL y contención).
   Razón: la estimación pesa más que el modelo de costo (Leis et al. 2015) y las muestras capturan predicados correlacionados y `LIKE` sin histogramas.
8. **Modelo de ejecución.**
   Decisión: §12 (vectores de 8192, morsels de 16 gránulos, despachador atómico, agregación radix en 64 particiones).
   Razón: alinea vector con gránulo y morsel con unidad de lectura, como el modelo de Leis et al. 2014.
9. **Subconjunto de SQL.**
   Decisión: §10, extraído de los archivos fijados de ClickBench (dialecto ClickHouse) y de las 22 consultas de DuckDB v2.0.0.
   Razón: es lo mínimo que corre las 65 consultas sin reescribirlas.
10. **Datos y escala.**
    Decisión: `hits_0..hits_9` (10 M filas, 1.49 GB) y TPC-H SF1 (respuestas oficiales) y SF10 (escala mayor).
    Razón: ClickHouse ocupa 1.47 GB con esos datos y DuckDB 2.39 GB, y SF10 se genera con 4.2 GB de RSS; los tres motores caben de sobra en disco y cada consulta en 14 GiB visibles.
11. **Umbrales numéricos.**
    Decisión: §16.
    Razón: los cocientes usan la fórmula de ClickBench (`+0.01 s`) para no premiar ni castigar arranques de milisegundos; los demás dejan margen sobre lo medido en las referencias (§5 de RESEARCH) y siguen siendo exigentes.
12. **Reglas de comparación.**
    Decisión: §15.
    Razón: los empates en el borde de LIMIT aparecieron en 7 de las 43 consultas incluso entre los dos oráculos.
13. **Versiones y descarga.**
    Decisión: DuckDB v2.0.0 (LTS) y ClickHouse 26.8.34.13-lts, descargados por `fetch` de los releases de GitHub y verificados por sha256/sha512 (apéndice A).
    Razón: ambas son LTS vigentes el 2026-11-01 y los binarios fijados hacen reproducible cada comparación.
14. **Nombres y forma de los comandos.**
    Decisión: §13, §14.2 y §17.
    Razón: un solo punto de entrada (`python3 -m harness`) con un contrato de salida uniforme deja que los checkers lean el resultado sin interpretar texto libre.
15. **Catálogo de mutantes.**
    Decisión: §18 (los seis de la idea, como features de Cargo).
    Razón: una feature no se desincroniza del código como un parche y el binario normal nunca la incluye.
16. **Orden de fases.**
    Decisión: K1 almacenamiento y ClickBench correcto; K2 joins, optimizador y oráculos; K3 fusiones, recuperación, Parquet de escritura y compresión; K4 rendimiento, memoria y mutantes.
    Razón: cada K se apoya en lo verificado por el anterior y el rendimiento se mide al final, con todo el motor presente.
17. **Herramientas de medición.**
    Decisión: tiempo de pared del proceso para veta y tiempo reportado por cada referencia; RSS con `/usr/bin/time -f %M` (GNU time 1.10); líneas no vacías contadas por el arnés; bytes con `du -sb`.
    Razón: todas están en la máquina sin root, y medir a veta con reloj de pared incluido el arranque es conservador contra veta.
18. **Tamaño de `hits.parquet` y de cada `hits_N.parquet`.**
    Decisión: confirmado: 14 779 976 446 bytes el único; 32.1–346.1 MiB cada particionado; los 10 elegidos suman 1 486 924 682 bytes.
    Razón: medido con HEAD y descarga (RESEARCH §4).
19. **Codificaciones que escriben los oráculos.**
    Decisión: DELTA_BYTE_ARRAY queda en el alcance con otro oráculo: los dos archivos de `apache/parquet-testing` fijados (veta lee, se compara con DuckDB) y la escritura de veta que leen DuckDB y ClickHouse.
    Razón: ni DuckDB 2.0.0 ni ClickHouse 26.8 la escriben, pero ambos la leen (RESEARCH §3).
20. **Extensión tpch.**
    Decisión: no viene incluida; `fetch` la instala una vez con red en `$VETA_DATA/duckdb-ext` y verifica su sha256; después todo corre sin red.
    Razón: medido con `duckdb_extensions()` y `unshare -rn`.
21. **Filas o gránulos leídos por las referencias.**
    Decisión: no se comparan; la poda se demuestra con los contadores de veta y la igualdad de resultados con y sin poda.
    Razón: los contadores internos de cada motor no miden lo mismo, así que no habría una conclusión de equivalencia.
22. **Join Order Benchmark.**
    Decisión: no se añade.
    Razón: exige cargar IMDB y otras 113 consultas en los tres motores; queda como límite declarado de la estimación de cardinalidad.
23. **Si la vacante acepta candidatos fuera de EE. UU.**
    Decisión: no se puede cerrar en K0; queda para César, fuera del proyecto.
    Razón: la API pública solo dice `Remote` con país USA y no hay otra fuente consultable.

Decisiones abiertas por el diseño:

24. **Timestamps en segundos.**
    Decisión: `Timestamp` guarda segundos; la escritura Parquet usa MILLIS.
    Razón: ClickBench y TPC-H no tienen fracciones de segundo y así coincide el formato de salida de los tres motores.
25. **Semántica de cadenas y regex.**
    Decisión: bytes (no UTF-8) y `.` coincide con `\n`, como ClickHouse.
    Razón: veta corre el dialecto de ClickHouse; la diferencia con DuckDB se resuelve con la reescritura de §15.4.
26. **`AVG` exacto.**
    Decisión: suma en `i128`, como DuckDB.
    Razón: es el resultado matemáticamente correcto; la diferencia con ClickHouse se resuelve con §15.4.
27. **Desanidado en el binder.**
    Decisión: las subconsultas se desanidan aunque el optimizador esté apagado.
    Razón: sin desanidar, Q2, Q17, Q20 y Q21 serían bucles anidados inviables, y el oráculo encendido/apagado necesita terminar.
28. **Uso de `VETA_DATA` dentro del repo.**
    Decisión: `<repo>/.veta-data`, ignorado por git.
    Razón: `/tmp` es tmpfs de 7.5 GB y los datos superan eso.
29. **Licencia y archivos de terceros.**
    Decisión: el repo es Apache-2.0; las consultas y esquemas de ClickBench (CC BY-NC-SA 4.0) no se versionan, se descargan fijados.
    Razón: mezclar esa licencia en el árbol del producto lo contaminaría; las consultas TPC-H de DuckDB (MIT) también se descargan, por simetría.
30. **Los checkers de cada K exigen los binarios.**
    Decisión: en un checker de K, un comando que termina con 77 cuenta como FAIL.
    Razón: un K se cierra con evidencia; el 77 de la idea queda para el usuario que corre `all` sin los binarios.

## 20. Contradicciones resueltas

- El método saca los «mutantes como puerta de cierre» y la idea pide un catálogo de mutantes. Se resuelve a favor de la idea: el catálogo es un criterio del producto (comando `mutants`), no una puerta del método; ningún checker de K corre mutantes salvo el criterio `k4.mutants`.
- El método fija ~15 % de código de verificación sobre producto y la idea pide que el arnés mida menos que el motor. El arnés es producto (lo que el proyecto enseña); el presupuesto del 15 % se aplica a `vault/private/checks/`.
- La idea pide que DuckDB escriba cada codificación, incluida DELTA_BYTE_ARRAY, y DuckDB no la escribe: decisión 19.
- La idea pide el mismo resultado que ClickHouse y que DuckDB, y los oráculos discrepan en Q4 y Q29: §15.4.
- La idea dice «por debajo de 16 GB» y la máquina expone 14 GiB: el tope es 10 GiB.

## Apéndice A. Fijaciones que `fetch` descarga y verifica

| Destino | URL | Verificación |
|---|---|---|
| `bin/duckdb` | `https://github.com/duckdb/duckdb/releases/download/v2.0.0/duckdb-cli-linux-amd64.tar.gz` (miembro `duckdb`) | tarball sha256 `c6507206baf7793c6013dbd82368469a45f20dc0e17a6f92f204602f652ed2bc`; binario sha256 `c41f957db5645d545a9caef430994cf262d24fad9297fef70c691395aadb43fb` |
| `duckdb-ext/v2.0.0/linux_amd64/tpch.duckdb_extension` | `INSTALL tpch` con `extension_directories=['<VETA_DATA>/duckdb-ext']` | sha256 `2e6bdf1a41b907a59b886fdd048c885512410ed717e2474bd7a52a2bde32d42f` |
| `bin/clickhouse` | `https://github.com/ClickHouse/ClickHouse/releases/download/v26.8.34.13-lts/clickhouse-common-static-26.8.34.13-amd64.tgz` (miembro `clickhouse-common-static-26.8.34.13/usr/bin/clickhouse`) | tgz sha512 `e5c753b258679daa882747aaec10b351a85bcad1412af7957d4a4616dcf5b45663da219bd37fe0697fde2ae76dbed2ba5b414ce9a5c8044c6d1fa0142e03e4cc`; binario sha256 `3bc4d96279e811079a1a8c81620942e676f1f889ce39e585bb0cbec5f9c59ee9` |
| `clickbench/hits_{0..9}.parquet` | `https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_{N}.parquet` | sha256 de RESEARCH §4 |
| `clickbench/queries-clickhouse.sql` | `https://raw.githubusercontent.com/ClickHouse/ClickBench/7276a62f955d5eac22f32ce1fd546b3953b05122/clickhouse/queries.sql` | `a7d6673357348ee9680443216b6f26f30d1dce9f313b419d38502417b2c2a219` |
| `clickbench/create-clickhouse.sql` | `…/7276a62f955d5eac22f32ce1fd546b3953b05122/clickhouse/create.sql` | `c1082ee0d86467a7c66ce3059e37461333d7078b288935f7c93fd6d92102a3dd` |
| `clickbench/queries-duckdb.sql` | `…/7276a62f955d5eac22f32ce1fd546b3953b05122/duckdb/queries.sql` | `274ffe1c4f83baad2fc177bbb6773bbdab0db779faeb2970de8cc531292a5dc6` |
| `clickbench/create-duckdb.sql` | `…/7276a62f955d5eac22f32ce1fd546b3953b05122/duckdb/create.sql` | `8065807651b163ab7006b8e451d820e2f6ddc757a6c6bd033800fe1dbc3f81e7` |
| `tpch/queries/qNN.sql` | `https://raw.githubusercontent.com/duckdb/duckdb/d54cbbe3df17e8e5ce68312f483b4155836112aa/extension/tpch/dbgen/queries/qNN.sql` | tabla A.1 |
| `parquet-testing/delta_byte_array.parquet` | `https://raw.githubusercontent.com/apache/parquet-testing/eedfad3f086678d282a00d8ff3cf8932d5d29af5/data/delta_byte_array.parquet` | `a400b789aef5cde88551f25cdd9bba8f0ff0fe01c48ddc5303c26edf119ee279` |
| `parquet-testing/delta_encoding_optional_column.parquet` | `…/eedfad3f086678d282a00d8ff3cf8932d5d29af5/data/delta_encoding_optional_column.parquet` | `71f8f00b00ecc132a1cc5d534acca900d02d0fe4ba3525607b03b2bd06a56f1a` |

Tabla A.1, sha256 de `qNN.sql`:

```
q01 8aefe77a1c97459bc69acbbcdaf3b8637d8ea22dfc3ac7be732d4446da83b69b
q02 a2219893f952dd2a2586ab297d909ec2d440faabb5e692f24880158aa6877344
q03 1f120d790d4914652cbf3fbc0fa28ed6573e4adee61ae7164a8c3d99b7c6b7fe
q04 2a2be89752fc917bc71e1a59168ead9d00d40c40ea0bbe646206e414b372f888
q05 495def7debb02ab1def31da07fbc88b4796813b754832ce15d54a35dcb104577
q06 3638535fb92a6026b3359a2ad9e6ed7da9ad9080152513e6fd3818d1a853c29f
q07 5e8077121a42ca8ebf5ff54e5842a614e839da945b0331e3b63d9ccad2361201
q08 948338f54b97a2c49aea15f8c4d609ecf244d00ad7754edcea60b4748504a1ce
q09 af90ccbe30ece396209e6627c10c0086ed54208100357922ee404de8bb867e56
q10 0b48b00cffa3776b32e67e9041191dea25095e2a952ee5251990b9c8585031ed
q11 f313c766fac507c1204fb6f2bacd35719a61a11c0f69601302555150dddeda78
q12 3814c9f4b7a476ee894c477d43cedb299603b1ed0f5315790d52839c8da355bf
q13 4a4a0b2c5646b2cad8f220a8f4b73ccb9f0acd45542353d63c18a69227bc20a3
q14 e7ec7960a4c49219fc76dc03495a1530aeb30c796413dd1dc9d501b86934f222
q15 1cb57383eeff60261a1b4b2a141c71d8672729d743a7dfa74d9a4bcaf69c44d4
q16 45085e7672cb1500e625e048156fc55369e7a9bc91fd3647c3f7735a870c3ef8
q17 dfc7f00ac8a500c2ac2735b05b41b462bbe4af9ce7cd6c37e42d1bb9df4a02e1
q18 8c5076061727d974f96708f0da930fc58843eac297a80c7ad658d64d152a19bb
q19 341f97d1f74deb30aca2a8b0ee849772a3b0e098a06cf12266c63573cb985478
q20 fa96ad5cb897bf31ec9b96a6b2a0d1f7132e71e2f481321e6db16628bc3e5fd5
q21 b29463f30a64e45a795047ee6e557e6c3c1f01fc612acc997873594048e74b59
q22 d8ed6332e75f9193391000ba071e17adcf735a2d99c5a4da0f7c13d695ccbd42
```

`lz4` es el del sistema (`lz4 v1.10.0`); si falta, `compressor` termina con 77.
