use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::stat::Mode;
use parking_lot::Mutex;
use serde::Serialize;

const KMSG_PATH: &str = "/dev/kmsg";
const OOM_KILL_MARKER: &str = "Killed process ";
const UNKNOWN_PROCESS: &str = "Unknown";
const KMSG_OPEN_ATTEMPTS: usize = 5;
const KMSG_OPEN_RETRY_DELAY: Duration = Duration::from_secs(1);
const KMSG_MAX_READ_FAILURES: usize = 5;
const KMSG_HEALTHY_READ: Duration = Duration::from_secs(60);
const KMSG_RECOVERY_DELAY: Duration = Duration::from_secs(30 * 60);
const MAX_OOM_KILLS: usize = 32;
const KMSG_RECORD_MAX: usize = 8192;
const KMSG_POLL_TIMEOUT_MS: u16 = 1000;
const OOM_KILL_PREFIXES: [&str; 3] = [
    "Out of memory: ",
    "Out of memory (oom_kill_allocating_task): ",
    "Memory cgroup out of memory: ",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OomKill {
    pub seq: i64,
    pub process: String,
}

#[derive(Default)]
struct Kills {
    kills: VecDeque<OomKill>,
    caught_up: bool,
    last_unnamed: i64,
}

#[derive(Default)]
pub struct OomWatcher {
    state: Mutex<Kills>,
}

struct GaveUp {
    message: &'static str,
    quiet: bool,
    error: Errno,
}

struct Kmsg {
    fd: OwnedFd,
}

impl Kmsg {
    fn open(path: &str) -> nix::Result<Self> {
        nix::fcntl::open(
            path,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map(|fd| Self {
            fd: unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) },
        })
    }

    fn read(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        nix::unistd::read(self.fd.as_raw_fd(), buffer)
    }

    fn wait(&self) -> nix::Result<()> {
        loop {
            let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
            match nix::poll::poll(&mut fds, PollTimeout::from(KMSG_POLL_TIMEOUT_MS)) {
                Ok(0) | Err(Errno::EINTR) => continue,
                Ok(_) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl OomWatcher {
    pub fn start() -> Arc<Self> {
        let watcher = Arc::new(Self::default());
        let watching = watcher.clone();
        std::thread::Builder::new()
            .name("oom-watcher".to_owned())
            .spawn(move || watching.watch())
            .map_err(|e| tracing::error!(error = %e, "failed to start the out-of-memory watcher"))
            .ok();
        watcher
    }

    pub fn kills(&self) -> Option<Vec<OomKill>> {
        let state = self.state.lock();
        state.caught_up.then(|| state.kills.iter().cloned().collect())
    }

    fn watch(&self) {
        let mut gave_up = false;
        loop {
            let round = self.watch_round(gave_up);
            if !round.quiet {
                tracing::error!(error = %round.error, path = KMSG_PATH, retry_in = ?KMSG_RECOVERY_DELAY, "{}", round.message);
            }
            gave_up = true;
            std::thread::sleep(KMSG_RECOVERY_DELAY);
        }
    }

    fn watch_round(&self, mut recovering: bool) -> GaveUp {
        let mut failures = 0;
        loop {
            let log = match open_kernel_log() {
                Ok(log) => log,
                Err(error) => {
                    return GaveUp {
                        message: "Failed to open the kernel log, out-of-memory kills will not be reported",
                        quiet: recovering,
                        error,
                    };
                }
            };
            let start = Instant::now();
            let error = follow_kernel_log(
                |buffer| log.read(buffer),
                || log.wait(),
                |record| self.add(record),
                || {
                    if recovering {
                        recovering = false;
                        tracing::info!(
                            path = KMSG_PATH,
                            "Reading the kernel log again, out-of-memory kills are reported"
                        );
                    }
                    self.mark_caught_up();
                },
            );
            drop(log);
            self.mark_stopped();
            if start.elapsed() >= KMSG_HEALTHY_READ {
                failures = 0;
                recovering = false;
            }
            failures += 1;
            if failures >= KMSG_MAX_READ_FAILURES {
                return GaveUp {
                    message: "Stopped reading the kernel log, out-of-memory kills will not be reported",
                    quiet: recovering,
                    error,
                };
            }
            if !recovering {
                tracing::warn!(error = %error, path = KMSG_PATH, "Failed to read the kernel log, reopening it");
            }
            std::thread::sleep(KMSG_OPEN_RETRY_DELAY);
        }
    }

    fn mark_caught_up(&self) {
        self.state.lock().caught_up = true;
    }

    fn mark_stopped(&self) {
        let mut state = self.state.lock();
        state.caught_up = false;
        state.kills.clear();
    }

    fn add(&self, record: &[u8]) {
        if !contains(record, OOM_KILL_MARKER.as_bytes()) {
            return;
        }
        let record = String::from_utf8_lossy(record);
        let Some(parsed) = parse_oom_kill(&record) else {
            return;
        };
        let mut state = self.state.lock();
        if let Err(e) = &parsed.name
            && parsed.seq > state.last_unnamed
        {
            state.last_unnamed = parsed.seq;
            tracing::error!(error = %e, record = %record, "Failed to find the process name of an out-of-memory kill");
        }
        state.kills.push_back(OomKill {
            seq: parsed.seq,
            process: parsed.name.unwrap_or_else(|_| UNKNOWN_PROCESS.to_owned()),
        });
        while state.kills.len() > MAX_OOM_KILLS {
            state.kills.pop_front();
        }
    }
}

fn open_kernel_log() -> nix::Result<Kmsg> {
    (0..KMSG_OPEN_ATTEMPTS)
        .map(|attempt| {
            if attempt > 0 {
                std::thread::sleep(KMSG_OPEN_RETRY_DELAY);
            }
            Kmsg::open(KMSG_PATH)
        })
        .reduce(|opened, next| opened.or(next))
        .unwrap_or(Err(Errno::ENOENT))
}

fn follow_kernel_log(
    mut read: impl FnMut(&mut [u8]) -> nix::Result<usize>,
    mut wait: impl FnMut() -> nix::Result<()>,
    mut record: impl FnMut(&[u8]),
    mut caught_up: impl FnMut(),
) -> Errno {
    let mut buffer = vec![0; KMSG_RECORD_MAX];
    loop {
        match read(&mut buffer) {
            Ok(read) => record(&buffer[..read]),
            Err(Errno::EAGAIN) => {
                caught_up();
                if let Err(e) = wait() {
                    return e;
                }
            }
            Err(Errno::EPIPE | Errno::EINTR) => {}
            Err(e) => return e,
        }
    }
}

struct ParsedKill {
    seq: i64,
    name: Result<String, &'static str>,
}

fn parse_oom_kill(record: &str) -> Option<ParsedKill> {
    let (seq, message) = parse_kernel_record(record)?;
    let message = message.split('\n').next().unwrap_or_default();
    let victim = OOM_KILL_PREFIXES
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix)?.strip_prefix(OOM_KILL_MARKER))?;
    Some(ParsedKill {
        seq,
        name: victim_name(victim),
    })
}

fn victim_name(victim: &str) -> Result<String, &'static str> {
    let (_, rest) = victim
        .split_once(" (")
        .filter(|(pid, _)| go_atoi(pid))
        .ok_or("no process ID followed by a name")?;
    rest.rfind(") total-vm:")
        .map(|end| rest[..end].to_owned())
        .ok_or("no end to the process name")
}

fn parse_kernel_record(record: &str) -> Option<(i64, &str)> {
    let (prefix, message) = record.split_once(';')?;
    let fields: Vec<&str> = prefix.splitn(3, ',').collect();
    (fields.len() >= 3).then_some(())?;
    fields[0]
        .parse::<i64>()
        .ok()
        .filter(|priority| go_atoi(fields[0]) && (0..=7).contains(priority))?;
    go_atoi(fields[1]).then_some(())?;
    fields[1].parse::<i64>().ok().map(|seq| (seq, message))
}

fn go_atoi(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kill(seq: i64, process: &str) -> OomKill {
        OomKill {
            seq,
            process: process.to_owned(),
        }
    }

    #[test]
    fn parses_the_kernel_oom_kill_messages() {
        let parsed = parse_oom_kill(
            "3,843,52404240,-;Out of memory: Killed process 475 (python3) total-vm:2443300kB, anon-rss:1kB",
        )
        .unwrap();
        assert_eq!((parsed.seq, parsed.name), (843, Ok("python3".to_owned())));
        let cgroup = parse_oom_kill(
            "6,9,1,-;Memory cgroup out of memory: Killed process 7 (my (odd) name) total-vm:1kB\n SUBSYSTEM=x",
        )
        .unwrap();
        assert_eq!(cgroup.name, Ok("my (odd) name".to_owned()));
        let allocating =
            parse_oom_kill("3,10,1,-;Out of memory (oom_kill_allocating_task): Killed process 8 (a) total-vm:1kB")
                .unwrap();
        assert_eq!(allocating.name, Ok("a".to_owned()));
    }

    #[test]
    fn ignores_user_space_and_other_records() {
        assert!(parse_oom_kill("12,5,1,-;Out of memory: Killed process 1 (x) total-vm:1kB").is_none());
        assert!(parse_oom_kill("3,5,1,-;echo Out of memory: Killed process 1 (x) total-vm:1kB").is_none());
        assert!(parse_oom_kill("3,5;Out of memory: Killed process 1 (x) total-vm:1kB").is_none());
        assert!(parse_oom_kill("Out of memory: Killed process 1 (x) total-vm:1kB").is_none());
        assert!(parse_oom_kill("3,x,1,-;Out of memory: Killed process 1 (x) total-vm:1kB").is_none());
    }

    #[test]
    fn a_kill_without_a_readable_name_is_unknown() {
        let no_pid = parse_oom_kill("3,5,1,-;Out of memory: Killed process abc (x) total-vm:1kB").unwrap();
        let no_end = parse_oom_kill("3,6,1,-;Out of memory: Killed process 1 (x)").unwrap();
        assert_eq!(no_pid.name, Err("no process ID followed by a name"));
        assert_eq!(no_end.name, Err("no end to the process name"));

        let watcher = OomWatcher::default();
        watcher.add(b"3,6,1,-;Out of memory: Killed process 1 (x)");
        watcher.mark_caught_up();
        assert_eq!(watcher.kills(), Some(vec![kill(6, "Unknown")]));
    }

    #[test]
    fn keeps_the_latest_32_kills_and_reports_them_only_when_caught_up() {
        let watcher = OomWatcher::default();
        (1..=40).for_each(|seq| {
            watcher.add(format!("3,{seq},1,-;Out of memory: Killed process 1 (p{seq}) total-vm:1kB").as_bytes())
        });

        assert_eq!(watcher.kills(), None);
        watcher.mark_caught_up();
        let kills = watcher.kills().unwrap();
        assert_eq!(kills.len(), 32);
        assert_eq!(kills[0], kill(9, "p9"));
        watcher.mark_stopped();
        assert_eq!(watcher.kills(), None);
        watcher.mark_caught_up();
        assert_eq!(watcher.kills(), Some(Vec::new()));
    }

    #[test]
    fn follows_the_log_until_a_read_fails() {
        let mut reads = VecDeque::from([
            Ok(b"3,1,1,-;Out of memory: Killed process 1 (a) total-vm:1kB".to_vec()),
            Err(Errno::EPIPE),
            Err(Errno::EAGAIN),
            Ok(b"6,2,1,-;hello".to_vec()),
            Err(Errno::EIO),
        ]);
        let watcher = OomWatcher::default();
        let mut waits = 0;

        let error = follow_kernel_log(
            |buffer| {
                reads.pop_front().unwrap().map(|record| {
                    buffer[..record.len()].copy_from_slice(&record);
                    record.len()
                })
            },
            || {
                waits += 1;
                Ok(())
            },
            |record| watcher.add(record),
            || watcher.mark_caught_up(),
        );

        assert_eq!((error, waits), (Errno::EIO, 1));
        assert_eq!(watcher.kills(), Some(vec![kill(1, "a")]));
    }
}
