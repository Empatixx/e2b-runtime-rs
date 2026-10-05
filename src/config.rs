//! CLI flags compatible with Go's `flag` package, as used by upstream envd:
//! `-x` and `--x` are the same, values come as `-x=v` or `-x v`,
//! and a bool flag without a value means `true`.

#[derive(Debug, Clone)]
pub struct Config {
    pub is_not_fc: bool,
    pub port: u16,
    pub cgroup_root: String,
    pub no_cgroups: bool,
    pub verbose: bool,
    pub version: bool,
    pub commit: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            is_not_fc: false,
            port: 49983,
            cgroup_root: "/sys/fs/cgroup".into(),
            no_cgroups: false,
            verbose: false,
            version: false,
            commit: false,
        }
    }
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut cfg = Config::default();
    // skip(1) = drop the program name (args[0])
    let mut it = args.skip(1);

    while let Some(arg) = it.next() {
        let name = arg.trim_start_matches('-');
        // "-port=80" -> key "port", inline Some("80");  "-port" -> key "port", inline None
        let (key, inline) = match name.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (name, None),
        };

        match key {
            "isnotfc" => cfg.is_not_fc = parse_bool(key, inline)?,
            "no-cgroups" => cfg.no_cgroups = parse_bool(key, inline)?,
            "verbose" => cfg.verbose = parse_bool(key, inline)?,
            "version" => cfg.version = parse_bool(key, inline)?,
            "commit" => cfg.commit = parse_bool(key, inline)?,
            "port" => cfg.port = parse_u16(key, take_value(key, inline, &mut it)?)?,
            "cgroup-root" => cfg.cgroup_root = take_value(key, inline, &mut it)?,
            other => return Err(format!("flag provided but not defined: -{other}")),
        }
    }

    Ok(cfg)
}

/// Value flag: `-x=v` (inline) or `-x v` (next argument); a missing value is an error.
fn take_value(key: &str, inline: Option<String>, it: &mut impl Iterator<Item = String>) -> Result<String, String> {
    inline
        .or_else(|| it.next())
        .ok_or(format!("flag needs an argument: -{key}"))
}

fn parse_u16(key: &str, value: String) -> Result<u16, String> {
    value.parse().map_err(|_| format!("invalid value {value:?} for flag -{key}: not a valid port"))
}

/// Bool flag value: missing means `true` (`-verbose`), otherwise "true"/"false".
fn parse_bool(key: &str, inline: Option<String>) -> Result<bool, String> {
    match inline {
        None => Ok(true),
        Some(v) => v.parse::<bool>().map_err(|e| format!("invalid value {v:?} for flag -{key}: {e}")),
    }
}
