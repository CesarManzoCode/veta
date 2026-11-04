//! Subcommand dispatch for the `veta` binary (contract §13).
//!
//! `merge`, `merger`, `export` and `parquet-meta` answer `unsupported` until K3.

mod args;
mod codec_tools;
mod table_tools;

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use veta_core::error::{Result, VetaError};
use veta_core::format;
use veta_core::memory;
use veta_exec::{Database, ExecOptions, ExecStats, LoadInput, execute};
use veta_sql::{OptimizerOptions, Statement, bind_query, optimize, parse_statements};

use args::Parsed;

/// Subcommands recognized by `veta` (contract §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    Sql,
    Load,
    Merge,
    Merger,
    Recover,
    Check,
    Parts,
    Export,
    ParquetMeta,
    CodecRoundtrip,
    DumpColumns,
    CodecBench,
}

impl Subcommand {
    pub const ALL: [Subcommand; 12] = [
        Subcommand::Sql,
        Subcommand::Load,
        Subcommand::Merge,
        Subcommand::Merger,
        Subcommand::Recover,
        Subcommand::Check,
        Subcommand::Parts,
        Subcommand::Export,
        Subcommand::ParquetMeta,
        Subcommand::CodecRoundtrip,
        Subcommand::DumpColumns,
        Subcommand::CodecBench,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Subcommand::Sql => "sql",
            Subcommand::Load => "load",
            Subcommand::Merge => "merge",
            Subcommand::Merger => "merger",
            Subcommand::Recover => "recover",
            Subcommand::Check => "check",
            Subcommand::Parts => "parts",
            Subcommand::Export => "export",
            Subcommand::ParquetMeta => "parquet-meta",
            Subcommand::CodecRoundtrip => "codec-roundtrip",
            Subcommand::DumpColumns => "dump-columns",
            Subcommand::CodecBench => "codec-bench",
        }
    }

    pub fn parse(name: &str) -> Option<Subcommand> {
        Subcommand::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// Runs `veta` with the arguments after the program name.
pub fn run(args: &[String]) -> Result<()> {
    let Some(first) = args.first() else {
        return Err(VetaError::User(usage()));
    };
    let rest = &args[1..];
    match Subcommand::parse(first) {
        Some(Subcommand::Sql) => cmd_sql(rest),
        Some(Subcommand::Load) => cmd_load(rest),
        Some(Subcommand::Recover) => table_tools::cmd_recover(rest),
        Some(Subcommand::Check) => table_tools::cmd_check(rest),
        Some(Subcommand::Parts) => table_tools::cmd_parts(rest),
        Some(Subcommand::DumpColumns) => table_tools::cmd_dump_columns(rest),
        Some(Subcommand::CodecRoundtrip) => codec_tools::cmd_codec_roundtrip(rest),
        Some(Subcommand::CodecBench) => codec_tools::cmd_codec_bench(rest),
        Some(
            sub @ (Subcommand::Merge
            | Subcommand::Merger
            | Subcommand::Export
            | Subcommand::ParquetMeta),
        ) => Err(VetaError::User(format!("unsupported: {}", sub.name()))),
        None => Err(VetaError::User(format!(
            "unknown subcommand: {first}\n{}",
            usage()
        ))),
    }
}

fn usage() -> String {
    let names: Vec<&str> = Subcommand::ALL.iter().map(|s| s.name()).collect();
    format!(
        "usage: veta <subcommand> ...\nsubcommands: {}",
        names.join(", ")
    )
}

/// Output of `sql`: `--format tsv` has no header (contract §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Tsv,
    TsvHeader,
}

fn parse_format(parsed: &Parsed) -> Result<OutputFormat> {
    match parsed.value("--format") {
        None | Some("tsv") => Ok(OutputFormat::Tsv),
        Some("tsv-header") => Ok(OutputFormat::TsvHeader),
        Some(other) => Err(VetaError::user(format!("invalid --format: {other}"))),
    }
}

/// `--threads N`, `available_parallelism` by default.
fn parse_threads(parsed: &Parsed) -> Result<usize> {
    let threads = parsed
        .parsed::<usize>("--threads")?
        .unwrap_or_else(ExecOptions::default_threads);
    if threads == 0 {
        return Err(VetaError::user("--threads must be at least 1"));
    }
    Ok(threads)
}

fn parse_sql_args(args: &[String]) -> Result<Parsed> {
    Parsed::new(
        args,
        &[
            "--db",
            "--threads",
            "--format",
            "--join-order",
            "--memory-limit-mb",
        ],
        &[],
        &[
            "--no-optimizer",
            "--no-prune",
            "--stats",
            "--explain-analyze",
        ],
    )
}

/// Writes `bytes`; a reader that went away (`head`) is not a failure of the query.
fn write_stdout(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    match out.write_all(bytes) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

fn cmd_sql(args: &[String]) -> Result<()> {
    let parsed = parse_sql_args(args)?;
    let db = Database::open(parsed.required("--db")?);
    let threads = parse_threads(&parsed)?;
    let output_format = parse_format(&parsed)?;
    if let Some(order) = parsed.value("--join-order")
        && order != "cost"
        && order != "syntactic"
    {
        return Err(VetaError::user(format!("invalid --join-order: {order}")));
    }
    if let Some(limit) = parsed.parsed::<u64>("--memory-limit-mb")? {
        memory::global().set_limit_mb(limit);
    }
    if parsed.flag("--explain-analyze") {
        return Err(VetaError::unsupported("--explain-analyze"));
    }
    let [sql] = parsed.positional() else {
        return Err(VetaError::user("sql takes exactly one statement"));
    };
    let statements = parse_statements(sql)?;
    if statements.is_empty() {
        return Err(VetaError::user("syntax error: empty statement"));
    }
    let stats = ExecStats::default();
    // §11.3: `--no-optimizer` leaves only column pruning; `--no-prune` only stops the
    // skipping of parts, granules and row groups.
    let optimizer = OptimizerOptions {
        enabled: !parsed.flag("--no-optimizer"),
    };
    let options = ExecOptions {
        threads,
        prune: !parsed.flag("--no-prune"),
    };
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 16, stdout.lock());
    for statement in &statements {
        match statement {
            Statement::CreateTable(create) => {
                db.create_table(create)?;
            }
            Statement::Query(query) => {
                let plan = optimize(bind_query(&db, query)?, optimizer);
                let result = execute(&db, &plan, &options, &stats)?;
                let mut text = Vec::new();
                if output_format == OutputFormat::TsvHeader {
                    let names: Vec<&str> = result
                        .schema
                        .fields
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect();
                    format::write_header(&mut text, &names);
                }
                for chunk in &result.chunks {
                    format::write_chunk(&mut text, chunk);
                    write_stdout(&mut out, &text)?;
                    text.clear();
                }
                write_stdout(&mut out, &text)?;
            }
            Statement::Explain { .. } => return Err(VetaError::unsupported("EXPLAIN")),
        }
    }
    match out.flush() {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e.into()),
        _ => {}
    }
    if parsed.flag("--stats") {
        eprintln!("VETA_STATS {}", stats.to_json());
    }
    Ok(())
}

fn cmd_load(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(
        args,
        &["--db", "--table", "--batch-rows"],
        &["--parquet", "--tsv"],
        &[],
    )?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let table = parsed.required("--table")?;
    let paths =
        |flag: &str| -> Vec<PathBuf> { parsed.values(flag).iter().map(PathBuf::from).collect() };
    let input = match (paths("--parquet"), paths("--tsv")) {
        (parquet, tsv) if tsv.is_empty() && !parquet.is_empty() => LoadInput::Parquet(parquet),
        (parquet, tsv) if parquet.is_empty() && !tsv.is_empty() => LoadInput::Tsv(tsv),
        _ => {
            return Err(VetaError::user(
                "load takes either --parquet or --tsv files",
            ));
        }
    };
    let summary = veta_exec::load(&db, table, &input, parsed.parsed::<usize>("--batch-rows")?)?;
    println!("load: {} parts, {} rows", summary.parts, summary.rows);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_subcommand_round_trips_its_name() {
        for sub in Subcommand::ALL {
            assert_eq!(Subcommand::parse(sub.name()), Some(sub));
        }
        assert_eq!(Subcommand::parse("select"), None);
    }

    #[test]
    fn subcommands_of_k3_are_unsupported_with_exit_1() {
        for sub in [
            Subcommand::Merge,
            Subcommand::Merger,
            Subcommand::Export,
            Subcommand::ParquetMeta,
        ] {
            let err = run(&args(&[sub.name(), "--db", "x"])).unwrap_err();
            assert_eq!(err.to_string(), format!("unsupported: {}", sub.name()));
            assert_eq!(err.exit_code(), 1);
        }
    }

    #[test]
    fn missing_or_unknown_subcommand_is_a_user_error() {
        assert_eq!(run(&[]).unwrap_err().exit_code(), 1);
        let err = run(&args(&["frobnicate"])).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string()
                .starts_with("unknown subcommand: frobnicate")
        );
    }

    #[test]
    fn invalid_options_are_user_errors() {
        for bad in [
            &["sql", "SELECT 1"][..],
            &["sql", "--db", "d", "--threads", "0", "SELECT 1"],
            &["sql", "--db", "d", "--threads", "x", "SELECT 1"],
            &["sql", "--db", "d", "--format", "csv", "SELECT 1"],
            &["sql", "--db", "d", "--join-order", "magic", "SELECT 1"],
            &["sql", "--db", "d", "--wat", "SELECT 1"],
            &["sql", "--db", "d"],
            &["sql", "--db", "d", "SELECT 1", "SELECT 2"],
            &["load", "--db", "d", "--table", "t"],
            &[
                "load",
                "--db",
                "d",
                "--table",
                "t",
                "--tsv",
                "a",
                "--parquet",
                "b",
            ],
        ] {
            let err = run(&args(bad)).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{bad:?}: {err}");
        }
    }
}
