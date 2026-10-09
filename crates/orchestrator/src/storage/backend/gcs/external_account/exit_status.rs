use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const SIGNALS: [&str; 32] = [
    "",
    "hangup",
    "interrupt",
    "quit",
    "illegal instruction",
    "trace/breakpoint trap",
    "aborted",
    "bus error",
    "floating point exception",
    "killed",
    "user defined signal 1",
    "segmentation fault",
    "user defined signal 2",
    "broken pipe",
    "alarm clock",
    "terminated",
    "stack fault",
    "child exited",
    "continued",
    "stopped (signal)",
    "stopped",
    "stopped (tty input)",
    "stopped (tty output)",
    "urgent I/O condition",
    "CPU time limit exceeded",
    "file size limit exceeded",
    "virtual timer expired",
    "profiling timer expired",
    "window changed",
    "I/O possible",
    "power failure",
    "bad system call",
];

fn signal_name(signal: i32) -> String {
    usize::try_from(signal)
        .ok()
        .and_then(|index| SIGNALS.get(index))
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("signal {signal}"), |name| (*name).to_owned())
}

pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

pub fn exit_text(status: ExitStatus) -> String {
    let text = match (status.code(), status.signal(), status.stopped_signal()) {
        (Some(code), _, _) => format!("exit status {code}"),
        (None, Some(signal), _) => format!("signal: {}", signal_name(signal)),
        (None, None, Some(signal)) => format!("stop signal: {}", signal_name(signal)),
        (None, None, None) if status.continued() => "continued".to_owned(),
        (None, None, None) => "exit status -1".to_owned(),
    };
    match status.core_dumped() {
        true => format!("{text} (core dumped)"),
        false => text,
    }
}

pub fn start_failure(program: &str, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound if !program.contains('/') => format!(
            "exec: {}: executable file not found in $PATH",
            crate::go_strconv::quote(program)
        ),
        _ => format!("fork/exec {program}: {}", crate::go_os::reason(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn status_of(script: &str) -> ExitStatus {
        Command::new("sh").args(["-c", script]).status().unwrap()
    }

    #[test]
    fn exits_and_signals_read_like_go_process_states() {
        assert_eq!(exit_text(status_of("exit 3")), "exit status 3");
        assert_eq!(exit_code(status_of("exit 3")), 3);
        assert_eq!(exit_text(status_of("kill -9 $$")), "signal: killed");
        assert_eq!(exit_code(status_of("kill -9 $$")), -1);
        assert_eq!(exit_text(status_of("kill -15 $$")), "signal: terminated");
    }

    #[test]
    fn signals_beyond_the_table_are_numbered() {
        assert_eq!(signal_name(16), "stack fault");
        assert_eq!(signal_name(40), "signal 40");
    }

    #[test]
    fn missing_programs_read_like_exec_lookups() {
        let missing = Command::new("no-such-binary-e2b").status().unwrap_err();

        assert_eq!(
            start_failure("no-such-binary-e2b", &missing),
            "exec: \"no-such-binary-e2b\": executable file not found in $PATH"
        );
        assert_eq!(
            start_failure("/no/such", &Command::new("/no/such").status().unwrap_err()),
            "fork/exec /no/such: no such file or directory"
        );
    }
}
