use std::time::Duration;

use serde_json::{Value, json};

use crate::featureflags::FeatureFlagsBuilder;
use crate::go_flag::{Kind, Parsed, parse};

pub const PLACEHOLDER_ENVD_VERSION: &str = "1.0.0";
const DEFAULT_FPH_BENCH_ITERATIONS: i64 = 3;

pub const FLAGS: &[(&str, Kind)] = &[
    ("from-build", Kind::Value),
    ("to-build", Kind::Value),
    ("storage", Kind::Value),
    ("sandbox-dir", Kind::Value),
    ("iterations", Kind::Value),
    ("cold", Kind::Bool),
    ("no-prefetch", Kind::Bool),
    ("no-egress", Kind::Bool),
    ("disable-memfd", Kind::Bool),
    ("sync-wp", Kind::Bool),
    ("tracker-dirty", Kind::Bool),
    ("memfile-diff-dedup", Kind::Bool),
    ("in-place", Kind::Bool),
    ("defer-memory-export", Kind::Bool),
    ("v", Kind::Bool),
    ("console", Kind::Bool),
    ("firecracker", Kind::Value),
    ("vcpu", Kind::Value),
    ("max-vcpus", Kind::Value),
    ("envd-version", Kind::Value),
    ("cmd", Kind::Value),
    ("pause", Kind::Bool),
    ("signal-pause", Kind::Value),
    ("cmd-pause", Kind::Value),
    ("cmd-signal-pause", Kind::Value),
    ("optimize", Kind::Bool),
    ("fs-only", Kind::Bool),
    ("reboot", Kind::Bool),
    ("force-reboot", Kind::Bool),
    ("cpu-template", Kind::Value),
    ("shell", Kind::Bool),
    ("fph-timeout-ms", Kind::Value),
    ("reclaim", Kind::Bool),
    ("collapse-envd-heap", Kind::Bool),
    ("envd-binary-cache", Kind::Bool),
    ("preboot-fs-recovery", Kind::Bool),
    ("fph-bench", Kind::Bool),
    ("fph-bench-delay", Kind::Value),
    ("gdb", Kind::Bool),
    ("gdb-fc", Kind::Value),
    ("gdb-symbols", Kind::Value),
    ("gdb-socket", Kind::Value),
    ("gdb-exec", Kind::Value),
    ("gdb-script", Kind::Value),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseOptions {
    pub immediate: bool,
    pub signal_name: String,
    pub command: String,
    pub command_signal: String,
    pub storage_path: String,
    pub remote_storage: bool,
    pub new_build_id: String,
    pub iterations: i64,
    pub optimize: bool,
    pub fs_only: bool,
    pub in_place: bool,
}

impl PauseOptions {
    pub fn enabled(&self) -> bool {
        self.immediate || !self.signal_name.is_empty() || !self.command.is_empty() || !self.command_signal.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub cmd: String,
    pub iterations: i64,
    pub console: bool,
    pub firecracker_version: String,
    pub envd_version: EnvdVersion,
    pub vcpu: i64,
    pub max_vcpus: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FphBenchOptions {
    pub enabled: bool,
    pub workload: String,
    pub iterations: i64,
    pub delay: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GdbOptions {
    pub enabled: bool,
    pub fc_binary: String,
    pub symbols: String,
    pub socket: String,
    pub exec_commands: String,
    pub script: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvdVersion {
    pub value: String,
    pub given: bool,
}

impl EnvdVersion {
    pub fn resolve(flag: &str) -> Result<Self, String> {
        if flag.is_empty() {
            return Ok(Self::default());
        }
        crate::envd::version::is_at_least(flag, "0.0.0")
            .map(|_| Self {
                value: flag.to_owned(),
                given: true,
            })
            .map_err(|e| format!("invalid -envd-version {flag:?}: {e}"))
    }

    pub fn version(&self) -> &str {
        match self.given {
            true => &self.value,
            false => PLACEHOLDER_ENVD_VERSION,
        }
    }

    pub fn describe(&self) -> String {
        match self.given {
            true => format!("{} (-envd-version)", self.version()),
            false => format!(
                "{} (placeholder, not the guest's version — pass -envd-version)",
                self.version()
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub from_build: String,
    pub storage: String,
    pub storage_explicit: bool,
    pub sandbox_dir: String,
    pub iterations: i64,
    pub cold: bool,
    pub no_prefetch: bool,
    pub no_egress: bool,
    pub verbose: bool,
    pub shell: bool,
    pub reboot: bool,
    pub force_reboot: bool,
    pub pause: PauseOptions,
    pub run: RunOptions,
    pub fph_bench: FphBenchOptions,
    pub gdb: GdbOptions,
    pub overrides: Vec<(String, Value)>,
}

pub fn options(args: &[String]) -> Result<Options, String> {
    let flags = parse(FLAGS, args)?;
    let overrides = overrides(&flags)?;
    let from_build = flags.string("from-build", "");
    if from_build.is_empty() {
        return Err("-from-build required".to_owned());
    }
    let envd_version = EnvdVersion::resolve(&flags.string("envd-version", ""))?;
    let pause = pause_options(&flags)?;
    let run = RunOptions {
        cmd: flags.string("cmd", ""),
        iterations: flags.int("iterations")?,
        console: flags.bool("console"),
        firecracker_version: flags.string("firecracker", ""),
        envd_version,
        vcpu: flags.int("vcpu")?,
        max_vcpus: flags.int("max-vcpus")?,
    };
    let fph_bench = FphBenchOptions {
        enabled: flags.bool("fph-bench"),
        workload: flags.string("cmd-pause", ""),
        iterations: match (flags.bool("fph-bench"), run.iterations) {
            (true, iterations) if iterations <= 0 => DEFAULT_FPH_BENCH_ITERATIONS,
            (_, iterations) => iterations,
        },
        delay: flags.duration("fph-bench-delay")?,
    };
    let options = Options {
        from_build,
        storage: flags.string("storage", ".local-build"),
        storage_explicit: flags.visited("storage"),
        sandbox_dir: flags.string("sandbox-dir", ""),
        iterations: run.iterations,
        cold: flags.bool("cold"),
        no_prefetch: flags.bool("no-prefetch"),
        no_egress: flags.bool("no-egress"),
        verbose: flags.bool("v"),
        shell: flags.bool("shell"),
        reboot: flags.bool("reboot"),
        force_reboot: flags.bool("force-reboot"),
        pause,
        run,
        fph_bench,
        gdb: GdbOptions {
            enabled: flags.bool("gdb"),
            fc_binary: flags.string("gdb-fc", ""),
            symbols: flags.string("gdb-symbols", ""),
            socket: flags.string("gdb-socket", ""),
            exec_commands: flags.string("gdb-exec", ""),
            script: flags.string("gdb-script", ""),
        },
        overrides,
    };
    validate(&options, &flags)?;
    Ok(options)
}

fn pause_options(flags: &Parsed) -> Result<PauseOptions, String> {
    let storage = flags.string("storage", ".local-build");
    let mut pause = PauseOptions {
        immediate: flags.bool("pause"),
        signal_name: flags.string("signal-pause", ""),
        command: flags.string("cmd-pause", ""),
        command_signal: flags.string("cmd-signal-pause", ""),
        remote_storage: storage.starts_with("gs://"),
        storage_path: storage,
        new_build_id: flags.string("to-build", ""),
        iterations: flags.int("iterations")?,
        optimize: flags.bool("optimize"),
        fs_only: flags.bool("fs-only"),
        in_place: flags.bool("in-place") || flags.bool("defer-memory-export"),
    };
    if pause.enabled() && pause.new_build_id.is_empty() {
        pause.new_build_id = uuid::Uuid::new_v4().to_string();
    }
    Ok(pause)
}

fn validate(options: &Options, flags: &Parsed) -> Result<(), String> {
    let pause = &options.pause;
    let pause_count = [
        pause.immediate,
        !pause.signal_name.is_empty(),
        !pause.command.is_empty(),
        !pause.command_signal.is_empty(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    let command_mode = !options.run.cmd.is_empty();
    let pause_mode = pause_count > 0;
    let checks = [
        (
            pause_count > 1,
            "only one of -pause, -signal-pause, -cmd-pause, or -cmd-signal-pause can be specified",
        ),
        (command_mode && pause_mode, "-cmd is incompatible with pause flags"),
        (
            !options.fph_bench.enabled
                && options.iterations > 0
                && (!pause.signal_name.is_empty() || !pause.command.is_empty() || !pause.command_signal.is_empty()),
            "-signal-pause, -cmd-pause, and -cmd-signal-pause are incompatible with -iterations",
        ),
        (
            flags.visited("to-build") && !pause_mode,
            "-to-build requires a pause flag (-pause, -signal-pause, -cmd-pause, or -cmd-signal-pause)",
        ),
        (
            pause.optimize && !pause_mode,
            "-optimize requires a pause flag (-pause, -signal-pause, -cmd-pause, or -cmd-signal-pause)",
        ),
        (
            pause.optimize && options.iterations > 0,
            "-optimize is incompatible with -iterations (benchmarking doesn't upload)",
        ),
        (
            pause.fs_only && !pause_mode,
            "-fs-only requires a pause flag (-pause, -signal-pause, -cmd-pause, or -cmd-signal-pause)",
        ),
        (
            pause.fs_only && (pause.optimize || options.fph_bench.enabled),
            "-fs-only is incompatible with -optimize and -fph-bench (no memory snapshot)",
        ),
        (
            options.shell && (command_mode || pause_mode || options.iterations > 0),
            "-shell can only be used in interactive mode (no -cmd, no pause flags, no -iterations)",
        ),
        (
            options.fph_bench.enabled && (pause.command.is_empty() || flags.int("fph-timeout-ms")? > 0),
            "-fph-bench requires -cmd-pause and is incompatible with -fph-timeout-ms",
        ),
        (
            options.run.vcpu < 0 || options.run.max_vcpus < 0,
            "-vcpu and -max-vcpus must not be negative",
        ),
        (
            options.run.max_vcpus != 0 && options.run.vcpu != 0 && options.run.max_vcpus < options.run.vcpu,
            "-max-vcpus must be at least -vcpu",
        ),
    ];
    checks
        .iter()
        .find(|(failed, _)| *failed)
        .map_or(Ok(()), |(_, message)| Err((*message).to_owned()))
}

fn overrides(flags: &Parsed) -> Result<Vec<(String, Value)>, String> {
    let fph_timeout = flags.int("fph-timeout-ms")?;
    let sync_wp = ["sync-wp", "tracker-dirty", "in-place", "defer-memory-export"]
        .iter()
        .any(|name| flags.bool(name));
    let in_place = flags.bool("in-place") || flags.bool("defer-memory-export");
    let cpu_template = match flags.string("cpu-template", "").as_str() {
        "" => None,
        "none" => Some(cpu_template_override(b"{}")?),
        path => Some(cpu_template_override(
            &std::fs::read(path).map_err(|e| format!("read -cpu-template: {e}"))?,
        )?),
    };
    let entries: [(bool, &str, Value); 12] = [
        (
            fph_timeout > 0,
            "free-page-hinting-config",
            json!({"enabled": true, "pause": fph_timeout}),
        ),
        (
            flags.bool("reclaim"),
            "guest-pause-reclaim",
            json!({"sync": 500, "drop_caches": 200, "compact_memory": 1000, "fstrim": 500}),
        ),
        (flags.bool("disable-memfd"), "use-memfd", json!(false)),
        (sync_wp, "use-sync-wp", json!(true)),
        (in_place, "in-place-checkpoint", json!(true)),
        (flags.bool("defer-memory-export"), "defer-memory-export", json!(true)),
        (flags.bool("tracker-dirty"), "sync-wp-tracker-dirty", json!(true)),
        (
            flags.visited("envd-binary-cache"),
            "envd-binary-cache",
            json!(flags.bool("envd-binary-cache")),
        ),
        (flags.bool("collapse-envd-heap"), "collapse-envd-heap", json!(true)),
        (flags.bool("preboot-fs-recovery"), "preboot-fs-recovery", json!(true)),
        (
            flags.bool("memfile-diff-dedup"),
            "memfile-diff-dedup",
            json!({"enabled": true}),
        ),
        (
            cpu_template.is_some(),
            "reboot-cpu-template-override",
            cpu_template.clone().unwrap_or(Value::Null),
        ),
    ];
    Ok(entries
        .into_iter()
        .filter(|(set, _, _)| *set)
        .map(|(_, key, value)| (key.to_owned(), value))
        .collect())
}

pub fn cpu_template_override(raw: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim();
    let template = match trimmed.is_empty() {
        true => json!({}),
        false => match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Null) => json!({}),
            Ok(parsed) => parsed,
            Err(_) => return Err("-cpu-template: not valid JSON".to_owned()),
        },
    };
    Ok(json!({ "template": template }))
}

pub fn apply_overrides(builder: FeatureFlagsBuilder, overrides: &[(String, Value)]) -> FeatureFlagsBuilder {
    overrides.iter().fold(builder, |builder, (key, value)| {
        builder.override_flag(key, value.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn a_build_is_required() {
        assert_eq!(options(&args(&[])), Err("-from-build required".to_owned()));
    }

    #[test]
    fn pause_flags_exclude_each_other_and_commands() {
        assert_eq!(
            options(&args(&["-from-build", "b", "-pause", "-cmd-pause", "ls"])).unwrap_err(),
            "only one of -pause, -signal-pause, -cmd-pause, or -cmd-signal-pause can be specified"
        );
        assert_eq!(
            options(&args(&["-from-build", "b", "-pause", "-cmd", "ls"])).unwrap_err(),
            "-cmd is incompatible with pause flags"
        );
        assert_eq!(
            options(&args(&["-from-build", "b", "-to-build", "x"])).unwrap_err(),
            "-to-build requires a pause flag (-pause, -signal-pause, -cmd-pause, or -cmd-signal-pause)"
        );
    }

    #[test]
    fn a_pause_gets_a_new_build_id_and_flags_become_overrides() {
        let parsed = options(&args(&[
            "-from-build",
            "b",
            "-pause",
            "-in-place",
            "-memfile-diff-dedup",
        ]))
        .unwrap();

        assert!(uuid::Uuid::parse_str(&parsed.pause.new_build_id).is_ok());
        assert!(parsed.pause.in_place);
        assert_eq!(
            parsed.overrides,
            vec![
                ("use-sync-wp".to_owned(), json!(true)),
                ("in-place-checkpoint".to_owned(), json!(true)),
                ("memfile-diff-dedup".to_owned(), json!({"enabled": true})),
            ]
        );
    }

    #[test]
    fn the_envd_version_defaults_to_the_placeholder() {
        assert_eq!(EnvdVersion::resolve("").unwrap().version(), "1.0.0");
        assert_eq!(
            EnvdVersion::resolve("0.5.4").unwrap().describe(),
            "0.5.4 (-envd-version)"
        );
        assert!(EnvdVersion::resolve("banana").is_err());
    }

    #[test]
    fn a_cpu_template_is_wrapped_and_empty_means_none() {
        assert_eq!(cpu_template_override(b"  ").unwrap(), json!({"template": {}}));
        assert_eq!(cpu_template_override(b"null").unwrap(), json!({"template": {}}));
        assert_eq!(
            cpu_template_override(br#"{"kvm_capabilities":[]}"#).unwrap(),
            json!({"template": {"kvm_capabilities": []}})
        );
        assert!(cpu_template_override(b"{").is_err());
    }
}
