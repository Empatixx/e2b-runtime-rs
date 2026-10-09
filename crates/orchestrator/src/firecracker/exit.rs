use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitInfo {
    Exited { code: i32 },
    Signaled { signal: i32, core_dumped: bool },
}

impl ExitInfo {
    pub fn from_status(status: ExitStatus) -> Option<Self> {
        status
            .signal()
            .map(|signal| Self::Signaled {
                signal,
                core_dumped: status.core_dumped(),
            })
            .or_else(|| status.code().map(|code| Self::Exited { code }))
    }

    pub fn log_fields(&self) -> String {
        match self {
            Self::Signaled { signal, core_dumped } => {
                format!("fc_signal={} fc_core_dumped={core_dumped}", signal_text(*signal))
            }
            Self::Exited { code } => format!("fc_exit_code={code}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashCause {
    CleanExit,
    ExitError,
    ExternalSignal,
    Fault,
    MemoryHandlerFailed,
    RequestedSignal,
    Unknown,
}

impl CrashCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CleanExit => "clean_exit",
            Self::ExitError => "exit_error",
            Self::ExternalSignal => "external_signal",
            Self::Fault => "fault",
            Self::MemoryHandlerFailed => "memory_handler_failed",
            Self::RequestedSignal => "requested_signal",
            Self::Unknown => "unknown",
        }
    }

    pub fn classify(exit: Option<&ExitInfo>) -> Self {
        match exit {
            None => Self::Unknown,
            Some(ExitInfo::Signaled { signal, .. }) if is_fault_signal(*signal) => Self::Fault,
            Some(ExitInfo::Signaled { .. }) => Self::ExternalSignal,
            Some(ExitInfo::Exited { code: 0 }) => Self::CleanExit,
            Some(ExitInfo::Exited { code }) => {
                handled_signal(*code).map_or(Self::ExitError, |signal| match is_fault_signal(signal) {
                    true => Self::Fault,
                    false => Self::ExternalSignal,
                })
            }
        }
    }

    pub fn with_memory_handler(self, handler_failed: bool) -> Self {
        match (self, handler_failed) {
            (Self::RequestedSignal, true) => Self::MemoryHandlerFailed,
            (cause, _) => cause,
        }
    }
}

const SIGHUP: i32 = 1;
const SIGILL: i32 = 4;
const SIGTRAP: i32 = 5;
const SIGABRT: i32 = 6;
const SIGBUS: i32 = 7;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;
const SIGSYS: i32 = 31;

const FAULT_SIGNALS: [i32; 7] = [SIGBUS, SIGSEGV, SIGILL, SIGFPE, SIGABRT, SIGSYS, SIGTRAP];

const GO_SIGNAL_NAMES: [&str; 32] = [
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

fn is_fault_signal(signal: i32) -> bool {
    FAULT_SIGNALS.contains(&signal)
}

fn handled_signal(code: i32) -> Option<i32> {
    match code {
        148 => Some(SIGSYS),
        149 => Some(SIGBUS),
        150 => Some(SIGSEGV),
        156 => Some(SIGHUP),
        157 => Some(SIGILL),
        _ => None,
    }
}

fn signal_text(signal: i32) -> String {
    usize::try_from(signal)
        .ok()
        .filter(|index| *index > 0)
        .and_then(|index| GO_SIGNAL_NAMES.get(index))
        .map_or_else(|| format!("signal {signal}"), |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGKILL: i32 = 9;
    const SIGTERM: i32 = 15;

    fn signaled(signal: i32) -> ExitInfo {
        ExitInfo::Signaled {
            signal,
            core_dumped: false,
        }
    }

    #[test]
    fn a_missing_exit_is_unknown() {
        assert_eq!(CrashCause::classify(None), CrashCause::Unknown);
    }

    #[test]
    fn exit_zero_is_a_clean_exit() {
        assert_eq!(
            CrashCause::classify(Some(&ExitInfo::Exited { code: 0 })),
            CrashCause::CleanExit
        );
    }

    #[test]
    fn a_failing_exit_code_is_an_exit_error() {
        assert_eq!(
            CrashCause::classify(Some(&ExitInfo::Exited { code: 1 })),
            CrashCause::ExitError
        );
        assert_eq!(
            CrashCause::classify(Some(&ExitInfo::Exited { code: 153 })),
            CrashCause::ExitError
        );
    }

    #[test]
    fn firecrackers_signal_handler_codes_map_back_to_their_signal() {
        let cause = |code| CrashCause::classify(Some(&ExitInfo::Exited { code }));
        assert_eq!(cause(148), CrashCause::Fault);
        assert_eq!(cause(149), CrashCause::Fault);
        assert_eq!(cause(150), CrashCause::Fault);
        assert_eq!(cause(157), CrashCause::Fault);
        assert_eq!(cause(156), CrashCause::ExternalSignal);
    }

    #[test]
    fn fault_signals_are_faults_and_kill_signals_are_external() {
        assert_eq!(CrashCause::classify(Some(&signaled(SIGSEGV))), CrashCause::Fault);
        assert_eq!(CrashCause::classify(Some(&signaled(SIGABRT))), CrashCause::Fault);
        assert_eq!(
            CrashCause::classify(Some(&signaled(SIGKILL))),
            CrashCause::ExternalSignal
        );
        assert_eq!(
            CrashCause::classify(Some(&signaled(SIGTERM))),
            CrashCause::ExternalSignal
        );
    }

    #[test]
    fn only_a_requested_signal_becomes_a_memory_handler_failure() {
        assert_eq!(
            CrashCause::RequestedSignal.with_memory_handler(true),
            CrashCause::MemoryHandlerFailed
        );
        assert_eq!(
            CrashCause::ExternalSignal.with_memory_handler(true),
            CrashCause::ExternalSignal
        );
        assert_eq!(
            CrashCause::RequestedSignal.with_memory_handler(false),
            CrashCause::RequestedSignal
        );
    }

    #[test]
    fn exit_statuses_convert_like_wait_status() {
        assert_eq!(
            ExitInfo::from_status(ExitStatus::from_raw(3 << 8)),
            Some(ExitInfo::Exited { code: 3 })
        );
        assert_eq!(
            ExitInfo::from_status(ExitStatus::from_raw(9)),
            Some(ExitInfo::Signaled {
                signal: 9,
                core_dumped: false
            })
        );
        assert_eq!(
            ExitInfo::from_status(ExitStatus::from_raw(11 | 0x80)),
            Some(ExitInfo::Signaled {
                signal: 11,
                core_dumped: true
            })
        );
    }

    #[test]
    fn log_fields_name_the_signal_like_go() {
        assert_eq!(signaled(SIGKILL).log_fields(), "fc_signal=killed fc_core_dumped=false");
        assert_eq!(ExitInfo::Exited { code: 2 }.log_fields(), "fc_exit_code=2");
    }
}
