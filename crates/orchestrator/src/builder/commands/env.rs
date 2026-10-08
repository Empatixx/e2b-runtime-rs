use std::net::SocketAddr;

use super::{CommandRun, root_only};
use crate::builder::sandbox_command::run_with_output;
use crate::proto::TemplateStep;
use crate::template::metadata::Context;

const NIL_MAP_PANIC: &str = "assignment to entry in nil map";

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, mut context: Context) -> Result<Context, String> {
    validate(step)?;
    let mut env_vars = context.env_vars.clone();
    for [key, value] in step.args.as_chunks::<2>().0 {
        let evaluated = evaluate(run.envd, value)
            .await
            .map_err(|e| format!("failed to evaluate environment variable {key}: {e}"))?;
        env_vars.as_mut().expect(NIL_MAP_PANIC).insert(key.clone(), evaluated);
    }
    context.env_vars = env_vars;
    Ok(context)
}

fn validate(step: &TemplateStep) -> Result<(), String> {
    let kind = step.r#type.to_uppercase();
    match step.args.len() {
        0 => Err(format!("{kind} does not support passing no arguments")),
        count if count % 2 != 0 => Err(format!("{kind} requires both a key and value arguments")),
        _ => Ok(()),
    }
}

async fn evaluate(envd: SocketAddr, value: &str) -> Result<String, String> {
    run_with_output(envd, &printf_command(value), &root_only())
        .await
        .map(|chunks| chunks.into_iter().map(|chunk| chunk.stdout).collect())
}

fn printf_command(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace("$(", "\\$(");
    format!("printf \"%s\" \"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: &str, args: &[&str]) -> TemplateStep {
        TemplateStep {
            r#type: kind.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            ..TemplateStep::default()
        }
    }

    #[test]
    fn values_are_escaped_except_variable_expansion() {
        assert_eq!(
            printf_command(r#"${PATH}:"a" `b` $(c) \d"#),
            r#"printf "%s" "${PATH}:\"a\" \`b\` \$(c) \\d""#
        );
        assert_eq!(printf_command("l1\nl2"), "printf \"%s\" \"l1\nl2\"");
    }

    #[test]
    fn arguments_come_in_pairs() {
        assert_eq!(
            validate(&step("arg", &[])),
            Err("ARG does not support passing no arguments".to_owned())
        );
        assert_eq!(
            validate(&step("ENV", &["A", "1", "B"])),
            Err("ENV requires both a key and value arguments".to_owned())
        );
        assert_eq!(validate(&step("ENV", &["A", "1"])), Ok(()));
    }
}
