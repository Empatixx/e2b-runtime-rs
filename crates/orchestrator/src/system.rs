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

    #[test]
    fn all_ok_joins_every_error() {
        assert_eq!(all_ok([Ok(()), Ok(())]), Ok(()));
        assert_eq!(
            all_ok([Err("a".to_owned()), Ok(()), Err("b".to_owned())]),
            Err("a; b".to_owned())
        );
    }
}
