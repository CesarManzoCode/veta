//! Subcommand dispatch for the `veta` binary (contract §13).

use veta_core::error::{Result, VetaError};

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
    match Subcommand::parse(first) {
        Some(sub) => Err(VetaError::User(format!("unsupported: {}", sub.name()))),
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
    fn known_subcommands_are_unsupported_with_exit_1() {
        for sub in Subcommand::ALL {
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
}
