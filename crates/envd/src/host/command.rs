use std::process::Stdio;

pub struct CommandFailure {
    pub error: String,
    pub output: String,
}

pub async fn combined_output(program: &str, args: &[&str]) -> Result<String, CommandFailure> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| CommandFailure {
            error: match e.kind() {
                std::io::ErrorKind::NotFound => format!("exec: {program:?}: executable file not found in $PATH"),
                _ => e.to_string(),
            },
            output: String::new(),
        })?;
    let combined = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&combined).into_owned();
    match output.status.code() {
        Some(0) => Ok(text),
        Some(code) => Err(CommandFailure {
            error: format!("exit status {code}"),
            output: text,
        }),
        None => Err(CommandFailure {
            error: "signal: killed".to_owned(),
            output: text,
        }),
    }
}
