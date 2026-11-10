//! Command-line options: `--name value` and `--flag`, nothing else. Unknown options
//! are errors, so a typo cannot silently run with a default.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::str::FromStr;

pub struct Args(BTreeMap<String, String>);

impl Args {
    /// `options` take a value, `flags` do not.
    pub fn parse(argv: &[String], options: &[&str], flags: &[&str]) -> Result<Self> {
        let mut map = BTreeMap::new();
        let mut it = argv.iter();
        while let Some(arg) = it.next() {
            let name = arg
                .strip_prefix("--")
                .with_context(|| format!("unexpected argument {arg}"))?;
            let value = if flags.contains(&name) {
                String::new()
            } else if options.contains(&name) {
                it.next()
                    .with_context(|| format!("--{name} needs a value"))?
                    .clone()
            } else {
                let known: Vec<String> = options
                    .iter()
                    .chain(flags)
                    .map(|o| format!("--{o}"))
                    .collect();
                bail!("unknown option --{name} (known: {})", known.join(" "));
            };
            map.insert(name.to_owned(), value);
        }
        Ok(Self(map))
    }

    /// The value of `--name`, or `default` when it was not given.
    pub fn get<T: FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.0.get(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| anyhow::anyhow!("bad value for --{name}: {v}")),
        }
    }

    pub fn opt(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_options_and_flags() {
        let a = Args::parse(
            &argv("--port 9000 --synthetic"),
            &["port", "fps"],
            &["synthetic"],
        )
        .unwrap();
        assert_eq!(a.get("port", 8090u16).unwrap(), 9000);
        assert_eq!(a.get("fps", 60u32).unwrap(), 60);
        assert!(a.flag("synthetic") && !a.flag("port-x"));
        assert_eq!(a.opt("port"), Some("9000"));
        assert_eq!(a.opt("fps"), None);
    }

    #[test]
    fn rejects_what_it_does_not_know() {
        let parse = |s: &str| Args::parse(&argv(s), &["port"], &["synthetic"]);
        assert!(parse("--prot 9000").is_err(), "a typo is not ignored");
        assert!(parse("--port").is_err(), "a missing value");
        assert!(parse("serve").is_err(), "a stray word");
        assert!(parse("--port x").unwrap().get("port", 1u16).is_err());
    }
}
