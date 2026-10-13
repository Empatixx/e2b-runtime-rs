use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::options::GdbOptions;
use super::runner::Runner;

const MACROS: &str = include_str!("fc-debug.gdb");
const GDB_STUB_MARKER: &[u8] = b"FIRECRACKER_GDB_SOCKET";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(90);
const SOCKET_POLL: Duration = Duration::from_millis(50);
const FIRECRACKER_BINARY: &str = "firecracker";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GdbLayout {
    pub original_versions_dir: PathBuf,
    pub staging_versions_dir: PathBuf,
    pub kernels_dir: PathBuf,
    pub arch: String,
}

struct Artifacts {
    firecracker: PathBuf,
    symbols: PathBuf,
}

pub async fn gdb_mode(runner: &Runner, options: &GdbOptions, layout: &GdbLayout) -> Result<(), String> {
    look_path("gdb").ok_or("gdb not found on PATH")?;
    let artifacts = resolve_artifacts(runner, options, layout)?;
    let staged = layout
        .staging_versions_dir
        .join(&runner.sandbox.firecracker_version)
        .join(&layout.arch)
        .join(FIRECRACKER_BINARY);
    std::fs::create_dir_all(staged.parent().unwrap_or(Path::new("/")))
        .map_err(|e| format!("gdb fc staging dir: {e}"))?;
    copy_executable(&artifacts.firecracker, &staged).map_err(|e| format!("stage debug firecracker: {e}"))?;
    let launched = runner
        .node
        .binaries
        .firecracker_for(&runner.sandbox.firecracker_version);
    let armed = std::fs::read(&launched)
        .map_err(|e| format!("check staged firecracker: {e}"))?
        .windows(GDB_STUB_MARKER.len())
        .any(|window| window == GDB_STUB_MARKER);
    if !armed {
        return Err(format!(
            "firecracker to launch ({}) is not gdb-enabled (no FIRECRACKER_GDB_SOCKET); it must be built with --features gdb (see fc-versions build.sh)",
            launched.display()
        ));
    }
    let socket = match options.socket.is_empty() {
        true => std::env::temp_dir().join(format!("fc-gdb-{}.sock", now_nanos())),
        false => PathBuf::from(&options.socket),
    };
    let _ = std::fs::remove_file(&socket);
    unsafe { std::env::set_var("FIRECRACKER_GDB_SOCKET", &socket) };
    runner.node.debug.lock().resume_without_envd = true;
    println!("🚀 Resuming under gdb (guest held at entry breakpoint)...");
    let started_at = Instant::now();
    let session = drive(runner, options, &artifacts, &socket, &launched, started_at).await;
    let _ = std::fs::remove_file(&socket);
    session
}

async fn drive(
    runner: &Runner,
    options: &GdbOptions,
    artifacts: &Artifacts,
    socket: &Path,
    launched: &Path,
    started_at: Instant,
) -> Result<(), String> {
    let resume = runner.start_sandbox(None);
    tokio::pin!(resume);
    let bound = tokio::select! {
        resumed = &mut resume => {
            if let Ok(started) = &resumed {
                println!("🧹 Cleanup...");
                runner.close(&started.id).await;
            }
            return match resumed {
                Err(e) => Err(format!("resume: {e}")),
                Ok(_) => Err(format!("resume completed without binding gdb socket {}", socket.display())),
            };
        }
        bound = wait_for_socket(socket) => bound,
    };
    bound.map_err(|e| format!("gdb socket {} never appeared: {e}", socket.display()))?;
    println!("✅ FC bound gdb socket in {:?}", started_at.elapsed());
    let init_script = write_init_script(&artifacts.symbols, socket)?;
    print_context(
        launched,
        &runner.sandbox.firecracker_version,
        &artifacts.symbols,
        socket,
        &init_script,
    );
    let gdb = tokio::spawn(run_gdb(init_script.clone(), options.clone()));
    let resumed = resume.await;
    let ran = gdb.await.map_err(|e| e.to_string())?;
    if let Ok(started) = resumed {
        println!("🧹 Cleanup...");
        runner.close(&started.id).await;
    }
    let _ = std::fs::remove_file(&init_script);
    ran
}

fn resolve_artifacts(runner: &Runner, options: &GdbOptions, layout: &GdbLayout) -> Result<Artifacts, String> {
    let firecracker_version = &runner.sandbox.firecracker_version;
    let kernel_version = &runner.sandbox.kernel_version;
    let firecracker = resolve_local(
        &options.fc_binary,
        &arch_or_legacy(
            &layout.original_versions_dir,
            firecracker_version,
            &layout.arch,
            "firecracker-debug",
        ),
    );
    let symbols = resolve_local(
        &options.symbols,
        &arch_or_legacy(&layout.kernels_dir, kernel_version, &layout.arch, "vmlinux.debug"),
    );
    let missing: Vec<String> = [
        firecracker
            .as_ref()
            .err()
            .map(|e| format!("firecracker-debug (FC {firecracker_version}): {e}")),
        symbols
            .as_ref()
            .err()
            .map(|e| format!("vmlinux.debug (kernel {kernel_version}): {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "could not find gdb debug artifacts locally:\n  - {}\nfirecracker-debug must sit next to the snapshot's firecracker, and vmlinux.debug\nnext to its vmlinux.bin (the fc-versions/fc-kernels buckets; copy-build -gdb stages\nthem). Otherwise pass -gdb-fc / -gdb-symbols explicitly",
            missing.join("\n  - ")
        ));
    }
    Ok(Artifacts {
        firecracker: firecracker?,
        symbols: symbols?,
    })
}

pub fn arch_or_legacy(base: &Path, version: &str, arch: &str, file: &str) -> PathBuf {
    let arch_path = base.join(version).join(arch).join(file);
    match is_file(&arch_path) {
        true => arch_path,
        false => base.join(version).join(file),
    }
}

pub fn resolve_local(override_path: &str, local: &Path) -> Result<PathBuf, String> {
    match (
        override_path.is_empty(),
        is_file(Path::new(override_path)),
        is_file(local),
    ) {
        (false, true, _) => Ok(PathBuf::from(override_path)),
        (false, false, _) => Err(format!("override path {override_path} does not exist")),
        (true, _, true) => Ok(local.to_path_buf()),
        (true, _, false) => Err(format!("not present at {}", local.display())),
    }
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir())
}

fn macro_library() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fc-debug.gdb")))
        .filter(|path| is_file(path))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| MACROS.to_owned())
}

pub fn init_script(macros: &str, symbols: &Path, socket: &Path) -> String {
    format!(
        "set pagination off\nset confirm off\n{macros}\n# FC boots the uncompressed vmlinux ELF directly, so KASLR never relocates the image:\n# symbols sit at their link-time addresses (offset 0).\nadd-symbol-file {} -o 0x0\n# FC binds the gdb socket while still loading the snapshot, so its first packet\n# ack can lag past gdb's 2s default. That makes gdb retransmit qSupported, the\n# stub double-replies, and the reply stream desyncs (gdb aborts with\n# \"Remote replied unexpectedly to 'vMustReplyEmpty'\"). Raise the timeout so gdb\n# does not prematurely retransmit during connect.\nset remotetimeout 120\ntarget remote {}\n",
        symbols.display(),
        socket.display()
    )
}

fn write_init_script(symbols: &Path, socket: &Path) -> Result<PathBuf, String> {
    let file = tempfile::Builder::new()
        .prefix("fc-debug-init-")
        .suffix(".gdb")
        .tempfile()
        .map_err(|e| format!("write gdb init script: {e}"))?;
    std::fs::write(file.path(), init_script(&macro_library(), symbols, socket))
        .map_err(|e| format!("write gdb init script: {e}"))?;
    file.into_temp_path()
        .keep()
        .map_err(|e| format!("close gdb init script: {e}"))
}

fn print_context(launched: &Path, version: &str, symbols: &Path, socket: &Path, init_script: &Path) {
    println!("\n──────────────── gdb debug context ────────────────");
    println!("  debug firecracker : {} (version {version})", launched.display());
    println!(
        "  kernel symbols    : {} (link addresses; FC ELF boot, no KASLR slide)",
        symbols.display()
    );
    println!("  gdb socket        : {}", socket.display());
    println!("  gdb init script   : {}", init_script.display());
    println!("  attach manually   : gdb -q -x {}", init_script.display());
    println!("  macros            : fc-faults [N], fc-curr, fc-task <p>, fc-regions, fc-va <phys>");
    println!("────────────────────────────────────────────────────");
}

pub fn gdb_args(init_script: &Path, options: &GdbOptions) -> Vec<String> {
    let scripted = !options.exec_commands.is_empty() || !options.script.is_empty();
    let mode = match scripted {
        true => "-batch",
        false => "-q",
    };
    let script = (!options.script.is_empty()).then(|| ["-x".to_owned(), options.script.clone()]);
    let commands = options
        .exec_commands
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .flat_map(|command| ["-ex".to_owned(), command.to_owned()]);
    [mode.to_owned(), "-x".to_owned(), init_script.display().to_string()]
        .into_iter()
        .chain(script.into_iter().flatten())
        .chain(commands)
        .collect()
}

async fn run_gdb(init_script: PathBuf, options: GdbOptions) -> Result<(), String> {
    let scripted = !options.exec_commands.is_empty() || !options.script.is_empty();
    if !scripted {
        println!("🐞 launching gdb (VM shuts down on disconnect)...\n");
    }
    let status = tokio::process::Command::new("gdb")
        .args(gdb_args(&init_script, &options))
        .status()
        .await
        .map_err(|e| e.to_string())?;
    match status.success() {
        true => Ok(()),
        false => Err(status.to_string()),
    }
}

async fn wait_for_socket(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + SOCKET_TIMEOUT;
    loop {
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("timeout".to_owned());
        }
        tokio::time::sleep(SOCKET_POLL).await;
    }
}

fn look_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_file(candidate))
}

fn copy_executable(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(source, destination)?;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(exec_commands: &str, script: &str) -> GdbOptions {
        GdbOptions {
            exec_commands: exec_commands.to_owned(),
            script: script.to_owned(),
            ..GdbOptions::default()
        }
    }

    #[test]
    fn scripted_runs_are_batch_and_split_commands_like_go() {
        let init = Path::new("/tmp/init.gdb");

        assert_eq!(gdb_args(init, &options("", "")), vec!["-q", "-x", "/tmp/init.gdb"]);
        assert_eq!(
            gdb_args(init, &options("bt; info regs\n\n x/4x $sp ", "/s.gdb")),
            vec![
                "-batch",
                "-x",
                "/tmp/init.gdb",
                "-x",
                "/s.gdb",
                "-ex",
                "bt",
                "-ex",
                "info regs",
                "-ex",
                "x/4x $sp"
            ]
        );
    }

    #[test]
    fn the_init_script_loads_symbols_at_offset_zero_and_connects() {
        let script = init_script(
            "define fc-curr\nend",
            Path::new("/k/vmlinux.debug"),
            Path::new("/tmp/fc.sock"),
        );

        assert!(script.starts_with("set pagination off\nset confirm off\ndefine fc-curr\nend\n"));
        assert!(script.contains("add-symbol-file /k/vmlinux.debug -o 0x0\n"));
        assert!(script.ends_with("set remotetimeout 120\ntarget remote /tmp/fc.sock\n"));
    }

    #[test]
    fn artifacts_prefer_the_arch_layout_and_overrides_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("v1/amd64")).unwrap();
        std::fs::write(dir.path().join("v1/amd64/firecracker-debug"), b"x").unwrap();

        assert_eq!(
            arch_or_legacy(dir.path(), "v1", "amd64", "firecracker-debug"),
            dir.path().join("v1/amd64/firecracker-debug")
        );
        assert_eq!(
            arch_or_legacy(dir.path(), "v2", "amd64", "firecracker-debug"),
            dir.path().join("v2/firecracker-debug")
        );
        assert_eq!(
            resolve_local("/missing", dir.path()),
            Err("override path /missing does not exist".to_owned())
        );
        assert!(resolve_local("", &dir.path().join("nope")).is_err());
    }
}
