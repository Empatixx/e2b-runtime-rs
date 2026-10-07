mod events;
mod output;
mod registry;
mod spawn;

use std::sync::Arc;

use connectrpc::{
    ConnectError, ErrorCode, InboundStream, RequestContext, ServiceRequest, ServiceResult, ServiceStream,
};
use nix::sys::signal::Signal as UnixSignal;
use tokio_stream::StreamExt;

use crate::app::AppState;
use crate::auth::request_user;
use crate::proto::process::__buffa::oneof::process_input::Input;
use crate::proto::process::__buffa::oneof::stream_input_request::Event as InputEvent;
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

async fn handle_input(process: &spawn::Process, input: Option<Input>) -> Result<(), ConnectError> {
    match input {
        Some(Input::Stdin(data)) => process
            .write_stdin(&data)
            .await
            .map_err(|e| ConnectError::internal(format!("error writing to stdin: {e}"))),
        Some(Input::Pty(data)) => process
            .write_pty(&data)
            .await
            .map_err(|e| ConnectError::internal(format!("error writing to tty: {e}"))),
        None => Err(ConnectError::unimplemented("invalid input type <nil>")),
    }
}

#[allow(refining_impl_trait)]
impl Process for ProcessService {
    async fn list(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListRequest>,
    ) -> ServiceResult<ListResponse> {
        let processes = self
            .processes
            .all()
            .iter()
            .map(|process| ProcessInfo {
                pid: process.pid,
                tag: process.tag.clone(),
                config: process.config.clone().into(),
                ..Default::default()
            })
            .collect();
        connectrpc::Response::ok(ListResponse {
            processes,
            ..Default::default()
        })
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
        request: ServiceRequest<'_, UpdateRequest>,
    ) -> ServiceResult<UpdateResponse> {
        let request = request.to_owned_message();
        let process = self.processes.find(
            request
                .process
                .as_option()
                .and_then(|process| process.selector.as_ref()),
        )?;
        if let Some(pty) = request.pty.as_option() {
            process
                .resize(pty.size.rows, pty.size.cols)
                .await
                .map_err(|e| ConnectError::internal(format!("error resizing tty: {e}")))?;
        }
        connectrpc::Response::ok(UpdateResponse::default())
    }

    async fn stream_input(
        &self,
        _ctx: RequestContext,
        mut requests: InboundStream<StreamInputRequest>,
    ) -> ServiceResult<StreamInputResponse> {
        let mut process = None;
        while let Some(message) = requests.next().await {
            let message = message
                .map_err(|e| ConnectError::new(ErrorCode::Unknown, format!("error streaming input: {e}")))?
                .to_owned_message();
            match message.event {
                Some(InputEvent::Start(start)) => {
                    let selector = start.process.as_option().and_then(|process| process.selector.as_ref());
                    process = Some(self.processes.find(selector)?);
                }
                Some(InputEvent::Data(data)) => {
                    let target = process
                        .as_ref()
                        .ok_or_else(|| ConnectError::failed_precondition("process not selected"))?;
                    handle_input(target, data.input.into_option().and_then(|input| input.input)).await?;
                }
                Some(InputEvent::Keepalive(_)) => {}
                None => return Err(ConnectError::unimplemented("invalid event type <nil>")),
            }
        }
        connectrpc::Response::ok(StreamInputResponse::default())
    }

    async fn send_input(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SendInputRequest>,
    ) -> ServiceResult<SendInputResponse> {
        let request = request.to_owned_message();
        let process = self.processes.find(
            request
                .process
                .as_option()
                .and_then(|process| process.selector.as_ref()),
        )?;
        handle_input(&process, request.input.into_option().and_then(|input| input.input)).await?;
        connectrpc::Response::ok(SendInputResponse::default())
    }

    async fn send_signal(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SendSignalRequest>,
    ) -> ServiceResult<SendSignalResponse> {
        let request = request.to_owned_message();
        let process = self.processes.find(
            request
                .process
                .as_option()
                .and_then(|process| process.selector.as_ref()),
        )?;
        let signal = match request.signal.as_known() {
            Some(Signal::SIGNAL_SIGKILL) => UnixSignal::SIGKILL,
            Some(Signal::SIGNAL_SIGTERM) => UnixSignal::SIGTERM,
            Some(other) => return Err(ConnectError::unimplemented(format!("invalid signal: {other:?}"))),
            None => {
                let value = request.signal.to_i32();
                return Err(ConnectError::unimplemented(format!("invalid signal: {value}")));
            }
        };
        process
            .signal(signal)
            .map_err(|e| ConnectError::internal(format!("error sending signal: {e}")))?;
        connectrpc::Response::ok(SendSignalResponse::default())
    }

    async fn close_stdin(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CloseStdinRequest>,
    ) -> ServiceResult<CloseStdinResponse> {
        let request = request.to_owned_message();
        let process = self.processes.find(
            request
                .process
                .as_option()
                .and_then(|process| process.selector.as_ref()),
        )?;
        process
            .close_stdin()
            .await
            .map_err(|e| ConnectError::new(ErrorCode::Unknown, format!("error closing stdin: {e}")))?;
        connectrpc::Response::ok(CloseStdinResponse::default())
    }
}
