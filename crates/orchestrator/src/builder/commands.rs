pub mod copy;
pub mod env;
pub mod run;
pub mod script;
pub mod user;
pub mod workdir;

use std::future::Future;
use std::net::SocketAddr;

use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::logs::ScopedLog;
use super::sandbox_command::{CommandLog, ROOT, run_logged, run_logged_confirmed};
use crate::envd::process::CommandContext;
use crate::proto::{LogLevel, TemplateStep};
use crate::storage::backend::Storage;
use crate::template::metadata::Context;

pub const PROCESS_CANCELLED: &str = "context: context canceled";
pub const START_CANCELLED: &str =
    "error starting process: canceled: Post \"http://localhost:5007/process.Process/Start\": context canceled";
pub const REQUEST_CANCELLED: &str = "context canceled";
pub const PROCESS_DEADLINE: &str = "context: context deadline exceeded";

#[derive(Clone, Copy)]
pub struct CommandRun<'a> {
    pub envd: SocketAddr,
    pub log: &'a ScopedLog<'a>,
    pub level: LogLevel,
    pub prefix: &'a str,
    pub files: &'a Storage,
    pub cache_scope: &'a str,
    pub cancel: &'a CancellationToken,
}

impl CommandRun<'_> {
    pub async fn logged(&self, command: &str, context: &CommandContext) -> Result<(), String> {
        self.refuse_start_when_cancelled()?;
        self.until_cancelled(
            run_logged(self.envd, command, context, &self.output()),
            PROCESS_CANCELLED,
        )
        .await
    }

    pub async fn logged_until(&self, deadline: Instant, command: &str, context: &CommandContext) -> Result<(), String> {
        tokio::time::timeout_at(deadline, self.logged(command, context))
            .await
            .unwrap_or_else(|_| Err(PROCESS_DEADLINE.to_owned()))
    }

    pub async fn logged_confirmed(
        &self,
        command: &str,
        context: &CommandContext,
        started: oneshot::Sender<()>,
    ) -> Result<(), String> {
        self.refuse_start_when_cancelled()?;
        let output = self.output();
        let run = run_logged_confirmed(self.envd, command, context, started, &output);
        self.until_cancelled(run, PROCESS_CANCELLED).await
    }

    fn refuse_start_when_cancelled(&self) -> Result<(), String> {
        match self.cancel.is_cancelled() {
            true => Err(START_CANCELLED.to_owned()),
            false => Ok(()),
        }
    }

    fn output(&self) -> CommandLog<'_> {
        CommandLog {
            log: self.log,
            level: self.level,
            id: self.prefix,
        }
    }

    pub async fn until_cancelled<T>(
        &self,
        work: impl Future<Output = Result<T, String>>,
        cancelled: &str,
    ) -> Result<T, String> {
        tokio::select! {
            biased;
            done = work => done,
            () = self.cancel.cancelled() => Err(cancelled.to_owned()),
        }
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
            cancel: &CancellationToken::new(),
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

    #[tokio::test]
    async fn a_command_started_after_the_cancel_fails_like_a_go_start_error() {
        let log = BuildLog::default();
        let fields = BTreeMap::new();
        let scoped = log.scoped(&fields);
        let dir = tempfile::tempdir().unwrap();
        let files = Storage::local(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let run = CommandRun {
            envd: "127.0.0.1:1".parse().unwrap(),
            log: &scoped,
            level: LogLevel::Info,
            prefix: "builder 1/1",
            files: &files,
            cache_scope: "scope",
            cancel: &cancel,
        };
        let step = TemplateStep {
            r#type: "RUN".to_owned(),
            args: vec!["sleep 60".to_owned()],
            ..TemplateStep::default()
        };

        assert_eq!(
            execute(&run, &step, Context::default()).await,
            Err(format!("failed to run command 'sleep 60': {START_CANCELLED}"))
        );
    }
}
