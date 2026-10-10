use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, exit};

pub const HELPER_ENV: &str = "E2B_REEXEC_HELPER";
pub const SELF_EXECUTABLE: &str = "/proc/self/exe";

#[derive(Clone, Copy)]
pub struct Helper {
    pub name: &'static str,
    pub run: fn(&[String]) -> Result<(), String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    NotAHelper,
    Ran(Result<(), String>),
    Missing(String),
}

impl Helper {
    pub fn command(&self, executable: &Path, args: &[String]) -> Command {
        let mut command = Command::new(executable);
        command.arg(self.name).args(args).env(HELPER_ENV, self.name);
        command
    }
}

pub fn dispatch(helpers: &[Helper], args: &[String], marker: Option<&str>) -> Dispatch {
    let requested = args
        .first()
        .and_then(|name| helpers.iter().find(|helper| helper.name == name));
    match (requested, marker) {
        (Some(helper), _) => Dispatch::Ran((helper.run)(&args[1..])),
        (None, Some(name)) => Dispatch::Missing(name.to_owned()),
        (None, None) => Dispatch::NotAHelper,
    }
}

pub fn main(helpers: &[Helper]) {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .map(|arg| arg.unwrap_or_else(|raw| raw.to_string_lossy().into_owned()))
        .collect();
    let marker = std::env::var(HELPER_ENV).ok();
    match dispatch(helpers, &args, marker.as_deref()) {
        Dispatch::NotAHelper => {}
        Dispatch::Ran(Ok(())) => exit(0),
        Dispatch::Ran(Err(message)) => {
            eprintln!("{message}");
            exit(1)
        }
        Dispatch::Missing(name) => {
            eprintln!("reexec: started as helper {name:?} but no such helper is defined");
            exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(args: &[String]) -> Result<(), String> {
        match args {
            [value] if value == "fail" => Err("asked to fail".to_owned()),
            [_] => Ok(()),
            _ => Err("usage: VALUE".to_owned()),
        }
    }

    const RECORD: Helper = Helper {
        name: "reexec-test-record",
        run: record,
    };

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn the_named_helper_runs_with_the_remaining_args() {
        assert_eq!(
            dispatch(&[RECORD], &strings(&["reexec-test-record", "hello"]), None),
            Dispatch::Ran(Ok(()))
        );
    }

    #[test]
    fn a_helper_error_is_handed_back_with_its_message() {
        assert_eq!(
            dispatch(
                &[RECORD],
                &strings(&["reexec-test-record", "fail"]),
                Some("reexec-test-record")
            ),
            Dispatch::Ran(Err("asked to fail".to_owned()))
        );
    }

    #[test]
    fn a_program_that_is_not_a_helper_falls_through() {
        assert_eq!(
            dispatch(&[RECORD], &strings(&["-port", "49983"]), None),
            Dispatch::NotAHelper
        );
        assert_eq!(dispatch(&[RECORD], &[], None), Dispatch::NotAHelper);
    }

    #[test]
    fn a_marked_child_without_its_helper_is_refused() {
        assert_eq!(
            dispatch(&[RECORD], &strings(&["no-such-helper"]), Some("no-such-helper")),
            Dispatch::Missing("no-such-helper".to_owned())
        );
    }

    #[test]
    fn the_command_names_the_helper_and_marks_the_child() {
        let command = RECORD.command(Path::new("/proc/self/exe"), &strings(&["x", "y"]));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["reexec-test-record", "x", "y"]);
        let marker = command.get_envs().find(|(key, _)| *key == HELPER_ENV);
        assert_eq!(marker.and_then(|(_, value)| value), Some("reexec-test-record".as_ref()));
    }
}
