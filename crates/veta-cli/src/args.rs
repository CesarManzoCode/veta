//! Command-line options: `--name value`, `--name value…` and `--name`.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use veta_core::error::{Result, VetaError};

/// Arguments of a subcommand after parsing.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    values: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
    positional: Vec<String>,
}

/// Whether `arg` looks like an option. SQL may start with a `--` comment, which always has
/// more text after a space or a line break.
fn is_option(arg: &str) -> bool {
    arg.starts_with("--") && !arg.contains(char::is_whitespace)
}

impl Parsed {
    /// Parses `args`: `single` options take one value, `multi` options take every value up
    /// to the next option and `flags` take none. Anything else starting with `--` is an
    /// error.
    pub fn new(args: &[String], single: &[&str], multi: &[&str], flags: &[&str]) -> Result<Parsed> {
        let mut parsed = Parsed::default();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            i += 1;
            if single.contains(&arg) {
                let value = args
                    .get(i)
                    .ok_or_else(|| VetaError::user(format!("{arg} needs a value")))?;
                i += 1;
                parsed.values.insert(arg.to_string(), vec![value.clone()]);
            } else if multi.contains(&arg) {
                let values = parsed.values.entry(arg.to_string()).or_default();
                while let Some(value) = args.get(i).filter(|v| !is_option(v)) {
                    values.push(value.clone());
                    i += 1;
                }
            } else if flags.contains(&arg) {
                parsed.flags.insert(arg.to_string());
            } else if is_option(arg) {
                return Err(VetaError::user(format!("unknown option: {arg}")));
            } else {
                parsed.positional.push(arg.to_string());
            }
        }
        Ok(parsed)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    pub fn values(&self, name: &str) -> &[String] {
        self.values.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn required(&self, name: &str) -> Result<&str> {
        self.value(name)
            .ok_or_else(|| VetaError::user(format!("missing {name}")))
    }

    /// The value of `name` converted to `T`, if present.
    pub fn parsed<T: FromStr>(&self, name: &str) -> Result<Option<T>> {
        self.value(name)
            .map(|text| {
                text.parse::<T>()
                    .map_err(|_| VetaError::user(format!("invalid {name}: {text}")))
            })
            .transpose()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    pub fn no_positional(&self) -> Result<()> {
        match self.positional.first() {
            Some(extra) => Err(VetaError::user(format!("unexpected argument: {extra}"))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Parsed> {
        let args: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        Parsed::new(&args, &["--db", "--n"], &["--files"], &["--stats"])
    }

    #[test]
    fn options_values_and_positionals() {
        let p = parse(&[
            "--db",
            "d",
            "--files",
            "a",
            "b",
            "--stats",
            "--n",
            "7",
            "-- c\nSELECT 1",
        ])
        .unwrap();
        assert_eq!(p.value("--db"), Some("d"));
        assert_eq!(p.values("--files"), &["a".to_string(), "b".to_string()]);
        assert!(p.flag("--stats"));
        assert_eq!(p.parsed::<u32>("--n").unwrap(), Some(7));
        assert_eq!(p.positional(), &["-- c\nSELECT 1".to_string()]);
        assert_eq!(p.required("--missing").unwrap_err().exit_code(), 1);
        assert!(p.no_positional().is_err());
    }

    #[test]
    fn bad_options_are_errors() {
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["--db"]).is_err());
        let p = parse(&["--n", "x"]).unwrap();
        assert_eq!(p.parsed::<u32>("--n").unwrap_err().exit_code(), 1);
        assert_eq!(parse(&[]).unwrap().parsed::<u32>("--n").unwrap(), None);
    }
}
