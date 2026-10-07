use std::process::Command;

pub fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let command_line = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{command_line}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let code = output
        .status
        .code()
        .map_or("signal".to_owned(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("{command_line}: exit {code}: {}", stderr.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_failure_with_stderr() {
        assert_eq!(run("true", &[]), Ok(()));
        let error = run("sh", &["-c", "echo boom >&2; exit 3"]).unwrap_err();
        assert_eq!(error, "sh -c echo boom >&2; exit 3: exit 3: boom");
        assert!(
            run("definitely-not-a-program", &[])
                .unwrap_err()
                .starts_with("definitely-not-a-program: ")
        );
    }
}
