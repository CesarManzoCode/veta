//! `export` and `parquet-meta` (contract §9, §13).

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use veta_core::error::{Result, VetaError};
use veta_core::{Value, format};
use veta_exec::{Database, ExecOptions, ExecStats, execute_streaming};
use veta_parquet::{
    Compression, PageVersion, ParquetFile, ParquetWriter, ValueEncoding, WriterOptions,
};
use veta_sql::{OptimizerOptions, Statement, bind_query, optimize, parse_statements};

use crate::args::Parsed;

/// Output buffer of `export`.
const OUTPUT_BUFFER: usize = 1 << 20;

/// `--name` converted by `parse`, the default when absent.
fn option<T>(parsed: &Parsed, name: &str, default: T, parse: fn(&str) -> Option<T>) -> Result<T> {
    match parsed.value(name) {
        None => Ok(default),
        Some(text) => parse(text).ok_or_else(|| VetaError::user(format!("invalid {name}: {text}"))),
    }
}

fn writer_options(parsed: &Parsed) -> Result<WriterOptions> {
    let defaults = WriterOptions::default();
    let row_group_rows = parsed
        .parsed::<usize>("--row-group-rows")?
        .unwrap_or(defaults.row_group_rows);
    if row_group_rows == 0 {
        return Err(VetaError::user("--row-group-rows must be at least 1"));
    }
    Ok(WriterOptions {
        encoding: option(
            parsed,
            "--encoding",
            defaults.encoding,
            ValueEncoding::parse,
        )?,
        compression: option(
            parsed,
            "--compression",
            defaults.compression,
            Compression::parse,
        )?,
        page_version: option(
            parsed,
            "--page-version",
            defaults.page_version,
            PageVersion::parse,
        )?,
        row_group_rows,
    })
}

/// The unfinished output, a sibling of the final file that a failed export does not leave
/// behind: `--out` only ever names a complete file.
struct TempOutput {
    path: PathBuf,
    keep: bool,
}

impl TempOutput {
    fn beside(out: &Path) -> Result<TempOutput> {
        let mut name = out
            .file_name()
            .ok_or_else(|| VetaError::user(format!("invalid --out: {}", out.display())))?
            .to_os_string();
        name.push(format!(".tmp.{}", std::process::id()));
        Ok(TempOutput {
            path: out.with_file_name(name),
            keep: false,
        })
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// `export --db D --sql "<SELECT>" --out F [...]`: runs the query and writes its rows as they
/// come, one row group at a time.
pub(crate) fn cmd_export(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(
        args,
        &[
            "--db",
            "--sql",
            "--out",
            "--encoding",
            "--compression",
            "--page-version",
            "--row-group-rows",
        ],
        &[],
        &[],
    )?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let sql = parsed.required("--sql")?;
    let out = PathBuf::from(parsed.required("--out")?);
    let options = writer_options(&parsed)?;
    let statements = parse_statements(sql)?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return Err(VetaError::user("export takes exactly one SELECT"));
    };
    let plan = optimize(bind_query(&db, query)?, OptimizerOptions::default(), &db);
    let schema = plan.schema();

    let mut temp = TempOutput::beside(&out)?;
    let file = File::create(&temp.path)
        .map_err(|e| VetaError::Io(format!("{}: {e}", temp.path.display())))?;
    let output = BufWriter::with_capacity(OUTPUT_BUFFER, file);
    let mut writer = ParquetWriter::new(output, &schema, options)?;
    let exec = ExecOptions {
        threads: ExecOptions::default_threads(),
        prune: true,
    };
    execute_streaming(&db, &plan, &exec, &ExecStats::default(), &mut |chunk| {
        writer.write_chunk(chunk)
    })?;
    writer.finish()?;
    fs::rename(&temp.path, &out).map_err(|e| VetaError::Io(format!("{}: {e}", out.display())))?;
    temp.keep = true;
    Ok(())
}

/// `parquet-meta --file F`: `row_group, column, physical type, encodings, compression, min,
/// max, null_count` of each column chunk. Encodings are comma-separated; what the footer does
/// not say is NULL (§13.1).
pub(crate) fn cmd_parquet_meta(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--file"], &[], &[])?;
    parsed.no_positional()?;
    let file = ParquetFile::open(parsed.required("--file")?)?;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut row = Vec::new();
    for (g, group) in file.metadata().row_groups.iter().enumerate() {
        for (c, chunk) in group.columns.iter().enumerate() {
            let md = chunk
                .meta_data
                .as_ref()
                .ok_or_else(|| VetaError::internal("column chunk without metadata"))?;
            let encodings: Vec<String> = md.encodings.iter().map(|e| e.name()).collect();
            let (min, max) = file
                .chunk_bounds(g, c)
                .unwrap_or((Value::Null, Value::Null));
            let nulls = md
                .statistics
                .as_ref()
                .and_then(|s| s.null_count)
                .map_or(Value::Null, Value::Int64);
            row.clear();
            format::write_row(
                &mut row,
                &[
                    Value::Int64(g as i64),
                    Value::varchar(&file.schema().field(c).name),
                    Value::varchar(md.type_.name()),
                    Value::varchar(encodings.join(",")),
                    Value::varchar(md.codec.name()),
                    min,
                    max,
                    nulls,
                ],
            );
            match out.write_all(&row) {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                other => other?,
            }
        }
    }
    match out.flush() {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}
