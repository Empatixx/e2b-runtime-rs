pub mod copy;
pub mod env;
pub mod run;
pub mod script;
pub mod user;
pub mod workdir;

use std::net::SocketAddr;

use super::logs::ScopedLog;
use super::sandbox_command::{CommandLog, ROOT, run_logged};
use crate::envd::process::CommandContext;
use crate::proto::{LogLevel, TemplateStep};
use crate::storage::backend::Storage;
use crate::template::metadata::Context;

#[derive(Clone, Copy)]
pub struct CommandRun<'a> {
    pub envd: SocketAddr,
    pub log: &'a ScopedLog<'a>,
    pub level: LogLevel,
    pub prefix: &'a str,
    pub files: &'a Storage,
    pub cache_scope: &'a str,
}

impl CommandRun<'_> {
    pub async fn logged(&self, command: &str, context: &CommandContext) -> Result<(), String> {
        let output = CommandLog {
            log: self.log,
            level: self.level,
            id: self.prefix,
        };
        run_logged(self.envd, command, context, &output).await
    }
}

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, context: Context) -> Result<Context, String> {
    match step.r#type.to_uppercase().as_str() {
        "ADD" | "COPY" => copy::execute(run, step, context).await,
        "RUN" => run::execute(run, step, context).await,
        "USER" => user::execute(run, step, context).await,
        "WORKDIR" => workdir::execute(run, step, context).await,
        "ENV" | "ARG" => env::execute(run, step, context).await,
        other => Err(format!(
            "failed to get command for step {}: command type {other} is not implemented",
            step.r#type
        )),
    }
}

pub fn user_of(context: &Context) -> String {
    context.user.clone().unwrap_or_default()
}

pub fn as_context(context: &Context) -> CommandContext {
    CommandContext {
        user: user_of(context),
        workdir: context.workdir.clone(),
        env_vars: context.env_vars.clone().unwrap_or_default(),
    }
}

pub fn as_root(context: &Context) -> CommandContext {
    CommandContext {
        user: ROOT.to_owned(),
        ..as_context(context)
    }
}

pub fn root_with_env(context: &Context) -> CommandContext {
    CommandContext {
        user: ROOT.to_owned(),
        workdir: None,
        env_vars: context.env_vars.clone().unwrap_or_default(),
    }
}

pub fn root_only() -> CommandContext {
    CommandContext {
        user: ROOT.to_owned(),
        ..CommandContext::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::builder::logs::BuildLog;

    #[tokio::test]
    async fn an_unknown_step_type_is_not_implemented() {
        let log = BuildLog::default();
        let fields = BTreeMap::new();
        let scoped = log.scoped(&fields);
        let dir = tempfile::tempdir().unwrap();
        let files = Storage::local(dir.path()).unwrap();
        let run = CommandRun {
            envd: "127.0.0.1:1".parse().unwrap(),
            log: &scoped,
            level: LogLevel::Info,
            prefix: "builder 1/1",
            files: &files,
            cache_scope: "scope",
        };
        let step = TemplateStep {
            r#type: "healthcheck".to_owned(),
            ..TemplateStep::default()
        };

        assert_eq!(
            execute(&run, &step, Context::default()).await,
            Err("failed to get command for step healthcheck: command type HEALTHCHECK is not implemented".to_owned())
        );
    }
}
