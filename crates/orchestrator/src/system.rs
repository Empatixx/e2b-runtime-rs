use tokio::process::Command;

pub async fn run(program: &str, args: &[&str]) -> Result<(), String> {
    output(program, args).await.map(|_| ())
}

pub async fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let command_line = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{command_line}: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let code = output
        .status
        .code()
        .map_or("signal".to_owned(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("{command_line}: exit {code}: {}", stderr.trim()))
}

pub async fn run_with_input(program: &str, args: &[&str], input: &str) -> Result<(), String> {
    use std::process::Stdio;

    use tokio::io::AsyncWriteExt;

    let command_line = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{command_line}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("{command_line}: write stdin: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("{command_line}: {e}"))?;
    match output.status.success() {
        true => Ok(()),
        false => Err(format!(
            "{command_line}: exit {}: {}",
            output
                .status
                .code()
                .map_or("signal".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

pub fn all_ok(results: impl IntoIterator<Item = Result<(), String>>) -> Result<(), String> {
    let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    errors.is_empty().then_some(()).ok_or_else(|| errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn success_and_failure_with_stderr() {
        assert_eq!(run("true", &[]).await, Ok(()));
        assert_eq!(output("echo", &["hi"]).await, Ok("hi\n".to_owned()));
        let error = run("sh", &["-c", "echo boom >&2; exit 3"]).await.unwrap_err();
        assert_eq!(error, "sh -c echo boom >&2; exit 3: exit 3: boom");
        let missing = run("definitely-not-a-program", &[]).await.unwrap_err();
        assert!(missing.starts_with("definitely-not-a-program: "));
    }

    #[tokio::test]
    async fn input_reaches_stdin() {
        assert_eq!(run_with_input("sh", &["-c", "grep -q hello"], "hello\n").await, Ok(()));
        assert!(run_with_input("sh", &["-c", "grep -q hello"], "bye\n").await.is_err());
    }

    #[test]
    fn all_ok_joins_every_error() {
        assert_eq!(all_ok([Ok(()), Ok(())]), Ok(()));
        assert_eq!(
            all_ok([Err("a".to_owned()), Ok(()), Err("b".to_owned())]),
            Err("a; b".to_owned())
        );
    }
}
