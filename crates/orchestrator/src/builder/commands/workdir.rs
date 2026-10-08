use std::net::SocketAddr;

use super::script::{Script, WORKDIR_SCRIPT};
use super::{CommandRun, PROCESS_CANCELLED, root_only, root_with_env, user_of};
use crate::builder::index::join_path;
use crate::envd::process::run_command;
use crate::proto::TemplateStep;
use crate::template::metadata::Context;

const RELATIVE_BASE: &str = "/";

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, mut context: Context) -> Result<Context, String> {
    let path = step.args.first().ok_or("WORKDIR requires a path argument")?;
    let target = target(context.workdir.as_deref(), path);
    let user = user_of(&context);
    let script = Script::new(WORKDIR_SCRIPT)
        .value("Target", &target)
        .value("User", &user)
        .render()?;
    run.logged(&script, &root_with_env(&context))
        .await
        .map_err(|e| format!("failed to create workdir: {e}"))?;
    context.workdir = Some(
        run.until_cancelled(absolute_path(run.envd, target), PROCESS_CANCELLED)
            .await?,
    );
    Ok(context)
}

fn target(current: Option<&str>, path: &str) -> String {
    match path.starts_with('/') {
        true => path.to_owned(),
        false => join_path(&[current.unwrap_or(RELATIVE_BASE), path]),
    }
}

async fn absolute_path(envd: SocketAddr, workdir: String) -> Result<String, String> {
    let command = format!("cd \"{workdir}\" && pwd");
    let mut resolved = workdir;
    let mut output_error = None;
    let ran = run_command(envd, &command, &root_only(), |stdout, stderr| match stderr.is_empty() {
        true => resolved = stdout.trim().to_owned(),
        false => output_error = Some(format!("error getting absolute path of workdir: {stderr}")),
    })
    .await;
    match (output_error, ran) {
        (Some(e), _) => Err(e),
        (None, Err(e)) => Err(format!("failed to save workdir {resolved}: {e}")),
        (None, Ok(())) => Ok(resolved),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_join_the_current_workdir_and_absolute_ones_stay_raw() {
        assert_eq!(target(None, "rel"), "/rel");
        assert_eq!(target(Some("/a/b"), "../x"), "/a/x");
        assert_eq!(target(Some("/a"), "./b/"), "/a/b");
        assert_eq!(target(Some("/a"), "/x/../y/"), "/x/../y/");
    }

    #[test]
    fn the_script_matches_upstream_bytes() {
        let script = Script::new(WORKDIR_SCRIPT)
            .value("Target", "/app")
            .value("User", "user")
            .render()
            .unwrap();

        assert_eq!(
            script,
            "\n\t\t\ttarget=\"/app\"\n\n\t\t\t# Exit early if target already exists\n\t\t\tif [ -d \"$target\" ]; then\n\t\t\t    exit 0\n\t\t\tfi\n\n\t\t\t# Find the first non-existent parent\n\t\t\tfirst_new=\"\"\n\t\t\tcheck=\"$target\"\n\t\t\twhile [ ! -d \"$check\" ]; do\n\t\t\t    first_new=\"$check\"\n\t\t\t    check=$(dirname \"$check\")\n\t\t\tdone\n\n\t\t\t# Create and chown from the top (only if we found new directories to create)\n\t\t\tif [ -n \"$first_new\" ]; then\n\t\t\t    mkdir -p \"$target\" && chown -R user:user \"$first_new\"\n\t\t\tfi\n    "
        );
    }
}
