mod events;
mod output;
mod registry;
mod spawn;

use std::sync::Arc;

use connectrpc::{ConnectError, InboundStream, RequestContext, ServiceRequest, ServiceResult, ServiceStream};

use crate::app::AppState;
use crate::auth::request_user;
use crate::proto::process::*;
use crate::user::User;
use crate::util::keepalive::keepalive_interval;
use events::{exited_events, live_events};
use registry::Processes;
use spawn::{Launch, spawn};

pub struct ProcessService {
    pub state: Arc<AppState>,
    pub processes: Arc<Processes>,
}

impl ProcessService {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            processes: Arc::default(),
        }
    }
}

fn start_response(event: ProcessEvent) -> StartResponse {
    StartResponse {
        event: event.into(),
        ..Default::default()
    }
}

fn connect_response(event: ProcessEvent) -> ConnectResponse {
    ConnectResponse {
        event: event.into(),
        ..Default::default()
    }
}

fn unimplemented(method: &str) -> ConnectError {
    ConnectError::unimplemented(format!("process.Process.{method} is not implemented"))
}

#[allow(refining_impl_trait)]
impl Process for ProcessService {
    async fn list(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListRequest>,
    ) -> ServiceResult<ListResponse> {
        Err(unimplemented("List"))
    }

    async fn connect(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, ConnectRequest>,
    ) -> ServiceResult<ServiceStream<ConnectResponse>> {
        let selector = request
            .to_owned_message()
            .process
            .into_option()
            .and_then(|process| process.selector);
        let interval = keepalive_interval(ctx.headers());

        match self.processes.find(selector.as_ref()) {
            Ok(process) => {
                let data = process.output.subscribe();
                connectrpc::Response::ok(live_events(process, data, interval, connect_response))
            }
            Err(not_found) => match self.processes.find_exited(selector.as_ref()) {
                Some(exited) => connectrpc::Response::ok(exited_events(exited.pid, exited.end, connect_response)),
                None => Err(not_found),
            },
        }
    }

    async fn start(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, StartRequest>,
    ) -> ServiceResult<ServiceStream<StartResponse>> {
        let defaults = self.state.defaults();
        let user = request_user(ctx.extensions().get::<User>(), &defaults.user)?;
        let launch = Launch {
            request: request.to_owned_message(),
            user,
            workdir: defaults.workdir,
            global_env: self.state.env_vars.all(),
            deadline: ctx.deadline(),
        };

        let (process, data) = spawn(launch, self.processes.clone())?;
        connectrpc::Response::ok(live_events(
            process,
            data,
            keepalive_interval(ctx.headers()),
            start_response,
        ))
    }

    async fn update(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, UpdateRequest>,
    ) -> ServiceResult<UpdateResponse> {
        Err(unimplemented("Update"))
    }

    async fn stream_input(
        &self,
        _ctx: RequestContext,
        _requests: InboundStream<StreamInputRequest>,
    ) -> ServiceResult<StreamInputResponse> {
        Err(unimplemented("StreamInput"))
    }

    async fn send_input(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, SendInputRequest>,
    ) -> ServiceResult<SendInputResponse> {
        Err(unimplemented("SendInput"))
    }

    async fn send_signal(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, SendSignalRequest>,
    ) -> ServiceResult<SendSignalResponse> {
        Err(unimplemented("SendSignal"))
    }

    async fn close_stdin(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, CloseStdinRequest>,
    ) -> ServiceResult<CloseStdinResponse> {
        Err(unimplemented("CloseStdin"))
    }
}
