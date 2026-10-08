use super::{CommandRun, as_context};
use crate::envd::process::CommandContext;
use crate::proto::TemplateStep;
use crate::template::metadata::Context;

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, context: Context) -> Result<Context, String> {
    let command = step.args.first().ok_or("RUN requires command argument")?;
    let as_user = step.args.get(1).map_or_else(
        || as_context(&context),
        |user| CommandContext {
            user: user.clone(),
            ..as_context(&context)
        },
    );
    run.logged(command, &as_user)
        .await
        .map_err(|e| format!("failed to run command '{command}': {e}"))?;
    Ok(context)
}
