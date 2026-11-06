//! `merge` and `merger` (contract §7, §13): recovery first (§6.5), then the merge policy of
//! `veta_storage::merge`, once in the foreground or for every table until killed.
//!
//! The background merger has nothing to tidy up when it gets SIGKILL or SIGTERM, whose
//! default action ends the process: what it leaves behind is what §6.5 cleans at the next
//! start. It writes nothing to stdout; an error with one table goes to stderr and that table
//! is tried again in the next round.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use veta_core::error::Result;
use veta_exec::Database;
use veta_storage::merge::{DEFAULT_MAX_MERGE_ROWS, DEFAULT_MERGE_MEMORY_MB};
use veta_storage::{DetachedPart, MergeOptions, MergeStep, Merger, recover};

use crate::args::Parsed;

/// Sleep of `merger` when no table has a window (contract §7).
const DEFAULT_INTERVAL_MS: u64 = 200;

fn merge_options(parsed: &Parsed, final_merge: bool) -> Result<MergeOptions> {
    Ok(MergeOptions {
        max_merge_rows: parsed
            .parsed::<u64>("--max-merge-rows")?
            .unwrap_or(DEFAULT_MAX_MERGE_ROWS),
        merge_memory_mb: parsed
            .parsed::<u64>("--merge-memory-mb")?
            .unwrap_or(DEFAULT_MERGE_MEMORY_MB),
        final_merge,
    })
}

fn report_detached(detached: &[DetachedPart]) {
    for part in detached {
        eprintln!("{}", part.message());
    }
}

/// `merge --db D --table T [--final] [--max-merge-rows N] [--merge-memory-mb N]`.
pub(crate) fn cmd_merge(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(
        args,
        &["--db", "--table", "--max-merge-rows", "--merge-memory-mb"],
        &[],
        &["--final"],
    )?;
    parsed.no_positional()?;
    let options = merge_options(&parsed, parsed.flag("--final"))?;
    let db = Database::open(parsed.required("--db")?);
    let name = parsed.required("--table")?;
    let table = db.open_table(name)?;
    report_detached(&recover(&table)?.detached);
    let mut merger = Merger::new(table, options);
    let result = merger.run();
    report_detached(&merger.take_detached());
    let summary = result?;
    println!(
        "merge: {name}: {} merges, {} active parts",
        summary.merges, summary.active
    );
    Ok(())
}

/// `merger --db D [--interval-ms N] [--max-merge-rows N] [--merge-memory-mb N]`: one merge
/// per table and round; it sleeps only after a round in which nothing merged.
pub(crate) fn cmd_merger(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(
        args,
        &[
            "--db",
            "--interval-ms",
            "--max-merge-rows",
            "--merge-memory-mb",
        ],
        &[],
        &[],
    )?;
    parsed.no_positional()?;
    let options = merge_options(&parsed, false)?;
    let interval = parsed
        .parsed::<u64>("--interval-ms")?
        .unwrap_or(DEFAULT_INTERVAL_MS);
    let db = Database::open(parsed.required("--db")?);
    let mut mergers = HashMap::new();
    loop {
        if !merger_round(&db, options, &mut mergers) {
            thread::sleep(Duration::from_millis(interval));
        }
    }
}

/// Tries one merge in every table; whether any happened.
fn merger_round(
    db: &Database,
    options: MergeOptions,
    mergers: &mut HashMap<String, Merger>,
) -> bool {
    let names = match db.table_names() {
        Ok(names) => names,
        Err(err) => {
            eprintln!("veta: {err}");
            return false;
        }
    };
    mergers.retain(|name, _| names.contains(name));
    let mut merged = false;
    for name in &names {
        match merge_table(db, name, options, mergers) {
            Ok(step) => merged |= step,
            Err(err) => eprintln!("veta: {err}"),
        }
    }
    merged
}

/// Tries one merge in table `name`, recovering the table the first time it is seen (a table
/// replaced by another `CREATE` counts as new). Whether a merge happened.
fn merge_table(
    db: &Database,
    name: &str,
    options: MergeOptions,
    mergers: &mut HashMap<String, Merger>,
) -> Result<bool> {
    let table = db.open_table(name)?;
    let is_new = mergers
        .get(name)
        .is_none_or(|merger| merger.table().def() != table.def());
    if is_new {
        report_detached(&recover(&table)?.detached);
        mergers.insert(name.to_string(), Merger::new(table, options));
    }
    let Some(merger) = mergers.get_mut(name) else {
        return Ok(false);
    };
    let step = merger.try_merge_once();
    report_detached(&merger.take_detached());
    Ok(matches!(step?, MergeStep::Merged(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use veta_sql::{Statement, parse_statement};

    fn parsed(list: &[&str]) -> Parsed {
        let args: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        Parsed::new(
            &args,
            &["--max-merge-rows", "--merge-memory-mb"],
            &[],
            &["--final"],
        )
        .unwrap()
    }

    #[test]
    fn options_have_the_defaults_of_the_contract() {
        let options = merge_options(&parsed(&[]), false).unwrap();
        assert_eq!(options.max_merge_rows, 20_000_000);
        assert_eq!(options.merge_memory_mb, 2048);
        assert!(!options.final_merge);
        let given = parsed(&["--max-merge-rows", "7", "--merge-memory-mb", "3"]);
        let options = merge_options(&given, true).unwrap();
        assert_eq!(options.max_merge_rows, 7);
        assert_eq!(options.merge_memory_mb, 3);
        assert!(options.final_merge);
        assert_eq!(
            merge_options(&parsed(&["--max-merge-rows", "x"]), false)
                .unwrap_err()
                .exit_code(),
            1
        );
    }

    #[test]
    fn a_round_merges_each_table_once_and_reports_when_idle() {
        let dir = std::env::temp_dir().join(format!("veta-merger-round-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let db = Database::open(&dir);
        let mut mergers = HashMap::new();
        let options = MergeOptions::default();
        // A database that does not exist yet has nothing to merge.
        assert!(!merger_round(&db, options, &mut mergers));

        let Statement::CreateTable(create) =
            parse_statement("CREATE TABLE t (k BIGINT NOT NULL, PRIMARY KEY (k))").unwrap()
        else {
            panic!("CREATE TABLE parses");
        };
        let table = db.create_table(&create).unwrap();
        let types = table.schema().data_types();
        for keys in [[1, 2], [3, 4]] {
            let rows: Vec<Vec<veta_core::Value>> = keys
                .iter()
                .map(|&k| vec![veta_core::Value::Int64(k)])
                .collect();
            let chunk = veta_core::Chunk::from_rows(&types, &rows).unwrap();
            veta_storage::insert(&table, &chunk).unwrap();
        }
        assert!(!mergers.contains_key("t"));
        assert!(merger_round(&db, options, &mut mergers));
        assert!(mergers.contains_key("t"));
        assert!(table.dir().join("all_1_2_1").is_dir());
        assert!(!table.dir().join("all_1_1_0").exists());
        assert!(!merger_round(&db, options, &mut mergers));

        assert!(veta_storage::Table::drop_table(&dir, "t").unwrap());
        assert!(!merger_round(&db, options, &mut mergers));
        assert!(mergers.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
