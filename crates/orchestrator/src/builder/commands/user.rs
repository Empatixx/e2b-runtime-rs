use std::net::SocketAddr;

use super::script::{ADMIN_GROUP_SCRIPT, Script};
use super::{CommandRun, PROCESS_CANCELLED, root_only, root_with_env};
use crate::builder::sandbox_command::run_quiet;
use crate::envd::process::{CommandContext, run_command};
use crate::proto::{LogLevel, TemplateStep};
use crate::template::metadata::Context;

const ADD_TO_SUDOERS: &str = "true";

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, context: Context) -> Result<Context, String> {
    let user = step.args.first().ok_or("USER requires a username argument")?;
    let as_root = root_with_env(&context);
    let exists = run
        .until_cancelled(
            run_quiet(run.envd, &format!("id -u {user}"), &as_root),
            PROCESS_CANCELLED,
        )
        .await;
    if exists.is_err() {
        run.logged(&format!("useradd --create-home --shell /bin/bash {user}"), &as_root)
            .await
            .map_err(|e| format!("failed to create user: {e}"))?;
    }
    if step.args.get(1).is_some_and(|sudo| sudo == ADD_TO_SUDOERS) {
        add_to_sudoers(run, user, &as_root).await?;
    }
    run.until_cancelled(save_user(run.envd, context, user), PROCESS_CANCELLED)
        .await
}

async fn add_to_sudoers(run: &CommandRun<'_>, user: &str, as_root: &CommandContext) -> Result<(), String> {
    let debug = CommandRun {
        level: LogLevel::Debug,
        ..*run
    };
    let admin_group = Script::new(ADMIN_GROUP_SCRIPT).value("User", user).render()?;
    debug
        .logged(&admin_group, as_root)
        .await
        .map_err(|e| format!("failed to add user to sudo group: {e}"))?;
    debug
        .logged(&format!("passwd -d {user}"), as_root)
        .await
        .map_err(|e| format!("failed to remove user password: {e}"))?;
    debug
        .logged(&sudoers_command(user), as_root)
        .await
        .map_err(|e| format!("failed to configure sudoers: {e}"))
}

fn sudoers_command(user: &str) -> String {
    format!(
        "grep -q '^{user} ALL=(ALL:ALL) NOPASSWD: ALL' /etc/sudoers || echo '{user} ALL=(ALL:ALL) NOPASSWD: ALL' >>/etc/sudoers"
    )
}

async fn save_user(envd: SocketAddr, mut context: Context, user: &str) -> Result<Context, String> {
    let mut saved = user.to_owned();
    run_command(envd, &format!("printf \"{user}\""), &root_only(), |stdout, _| {
        saved = stdout.to_owned()
    })
    .await?;
    context.user = Some(saved).filter(|saved| !saved.is_empty());
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scripts_name_the_user_everywhere() {
        let admin_group = Script::new(ADMIN_GROUP_SCRIPT).value("User", "dev").render().unwrap();

        assert_eq!(admin_group.matches("usermod -aG ").count(), 3);
        assert!(admin_group.contains("usermod -aG \"$E2B_ADMIN_GROUP\" dev\n"));
        assert!(admin_group.ends_with("    exit 1\nfi"));
        assert_eq!(
            sudoers_command("dev"),
            "grep -q '^dev ALL=(ALL:ALL) NOPASSWD: ALL' /etc/sudoers || echo 'dev ALL=(ALL:ALL) NOPASSWD: ALL' >>/etc/sudoers"
        );
    }
}
