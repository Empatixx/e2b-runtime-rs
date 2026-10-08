#[derive(Debug, Clone)]
pub struct Config {
    pub is_not_fc: bool,
    pub port: u16,
    pub cgroup_root: String,
    pub no_cgroups: bool,
    pub verbose: bool,
    pub version: bool,
    pub commit: bool,
    pub resume_handover: bool,
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
            resume_handover: false,
        }
    }
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = args.skip(1);

    while let Some(arg) = args.next() {
        let (name, inline) = match arg.trim_start_matches('-').split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (arg.trim_start_matches('-').to_owned(), None),
        };

        match name.as_str() {
            "isnotfc" => config.is_not_fc = bool_flag(&name, inline)?,
            "no-cgroups" => config.no_cgroups = bool_flag(&name, inline)?,
            "verbose" => config.verbose = bool_flag(&name, inline)?,
            "version" => config.version = bool_flag(&name, inline)?,
            "commit" => config.commit = bool_flag(&name, inline)?,
            "resume-handover" => config.resume_handover = bool_flag(&name, inline)?,
            "port" => {
                let value = flag_value(&name, inline, &mut args)?;
                config.port = value
                    .parse()
                    .map_err(|_| format!("invalid value {value:?} for flag -{name}: parse error"))?;
            }
            "cgroup-root" => config.cgroup_root = flag_value(&name, inline, &mut args)?,
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }

    Ok(config)
}

fn flag_value(name: &str, inline: Option<String>, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    inline
        .or_else(|| args.next())
        .ok_or_else(|| format!("flag needs an argument: -{name}"))
}

fn bool_flag(name: &str, inline: Option<String>) -> Result<bool, String> {
    inline.map_or(Ok(true), |value| match value.as_str() {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(format!("invalid boolean value {value:?} for -{name}: parse error")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(list: &[&str]) -> Result<Config, String> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn bool_spellings_and_errors() {
        assert!(parse_args(&["envd", "-verbose=T"]).unwrap().verbose);
        assert!(!parse_args(&["envd", "-verbose=0"]).unwrap().verbose);
        assert_eq!(
            parse_args(&["envd", "-verbose=yes"]).unwrap_err(),
            "invalid boolean value \"yes\" for -verbose: parse error"
        );
        assert_eq!(
            parse_args(&["envd", "-port=x"]).unwrap_err(),
            "invalid value \"x\" for flag -port: parse error"
        );
        assert_eq!(
            parse_args(&["envd", "--cgroup-root", "/cg"]).unwrap().cgroup_root,
            "/cg"
        );
    }
}
