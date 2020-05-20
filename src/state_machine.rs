// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    common::{App, AppSet, CheckOptions, CheckTiming},
    configuration::Config,
    http_request::HttpRequest,
    installer::{Installer, Plan},
    metrics::{Metrics, MetricsReporter, UpdateCheckFailureReason},
    policy::{CheckDecision, PolicyEngine, UpdateDecision},
    protocol::{
        self,
        request::{Event, EventErrorCode, EventResult, EventType},
        response::{parse_json_response, OmahaStatus, Response},
    },
    request_builder::{self, RequestBuilder, RequestParams},
    storage::{Storage, StorageExt},
    time::{
        system_time_conversion::{
            checked_system_time_to_micros_from_epoch, micros_from_epoch_to_system_time,
        },
        TimeSource, Timer,
    },
};

#[cfg(test)]
use crate::common::{ProtocolState, UpdateCheckSchedule};

use futures::{
    channel::{mpsc, oneshot},
    compat::Stream01CompatExt,
    future,
    lock::Mutex,
    prelude::*,
    select,
};
use http::response::Parts;
use log::{error, info, warn};
use std::rc::Rc;
use std::str::Utf8Error;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

pub mod update_check;

mod builder;
pub use builder::StateMachineBuilder;

mod observer;
use observer::StateMachineProgressObserver;
pub use observer::{InstallProgress, StateMachineEvent};

const LAST_CHECK_TIME: &str = "last_check_time";
const INSTALL_PLAN_ID: &str = "install_plan_id";
const UPDATE_FIRST_SEEN_TIME: &str = "update_first_seen_time";
const CONSECUTIVE_FAILED_UPDATE_CHECKS: &str = "consecutive_failed_update_checks";

/// This is the core state machine for a client's update check.  It is instantiated and used to
/// perform update checks over time or to perform a single update check process.
#[derive(Debug)]
pub struct StateMachine<PE, HR, IN, TM, MR, ST>
where
    PE: PolicyEngine,
    HR: HttpRequest,
    IN: Installer,
    TM: Timer,
    MR: MetricsReporter,
    ST: Storage,
{
    /// The immutable configuration of the client itself.
    config: Config,

    policy_engine: PE,

    http: HR,

    installer: IN,

    timer: TM,

    time_source: PE::TimeSource,

    metrics_reporter: MR,

    storage_ref: Rc<Mutex<ST>>,

    /// Context for update check.
    context: update_check::Context,

    /// The current State of the StateMachine.
    state: State,

    /// The list of apps used for update check.
    app_set: AppSet,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum State {
    Idle,
    CheckingForUpdates,
    ErrorCheckingForUpdate,
    NoUpdateAvailable,
    InstallationDeferredByPolicy,
    InstallingUpdate,
    WaitingForReboot,
    InstallationError,
}

/// This is the set of errors that can occur when making a request to Omaha.  This is an internal
/// collection of error types.
#[derive(Error, Debug)]
pub enum OmahaRequestError {
    #[error("Unexpected JSON error constructing update check: {}", _0)]
    Json(serde_json::Error),

    #[error("Error building update check HTTP request: {}", _0)]
    HttpBuilder(http::Error),

    #[error("Hyper error performing update check: {}", _0)]
    Hyper(hyper::Error),

    #[error("HTTP error performing update check: {}", _0)]
    HttpStatus(hyper::StatusCode),
}

impl From<request_builder::Error> for OmahaRequestError {
    fn from(err: request_builder::Error) -> Self {
        match err {
            request_builder::Error::Json(e) => OmahaRequestError::Json(e),
            request_builder::Error::Http(e) => OmahaRequestError::HttpBuilder(e),
        }
    }
}

impl From<hyper::Error> for OmahaRequestError {
    fn from(err: hyper::Error) -> Self {
        OmahaRequestError::Hyper(err)
    }
}

impl From<serde_json::Error> for OmahaRequestError {
    fn from(err: serde_json::Error) -> Self {
        OmahaRequestError::Json(err)
    }
}

impl From<http::Error> for OmahaRequestError {
    fn from(err: http::Error) -> Self {
        OmahaRequestError::HttpBuilder(err)
    }
}

impl From<http::StatusCode> for OmahaRequestError {
    fn from(sc: http::StatusCode) -> Self {
        OmahaRequestError::HttpStatus(sc)
    }
}

/// This is the set of errors that can occur when parsing the response body from Omaha.  This is an
/// internal collection of error types.
#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum ResponseParseError {
    #[error("Response was not valid UTF-8")]
    Utf8(Utf8Error),

    #[error("Unexpected JSON error parsing update check response: {}", _0)]
    Json(serde_json::Error),
}

#[derive(Error, Debug)]
pub enum UpdateCheckError {
    #[error("Check not performed per policy: {:?}", _0)]
    Policy(CheckDecision),

    #[error("Error checking with Omaha: {:?}", _0)]
    OmahaRequest(OmahaRequestError),

    #[error("Error parsing Omaha response: {:?}", _0)]
    ResponseParser(ResponseParseError),

    #[error("Unable to create an install plan: {:?}", _0)]
    InstallPlan(anyhow::Error),
}

/// A handle to interact with the state machine running in another task.
#[derive(Clone)]
pub struct ControlHandle(mpsc::Sender<ControlRequest>);

/// Error indicating that the state machine task no longer exists.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("state machine dropped before all its control handles")]
pub struct StateMachineGone;

impl From<mpsc::SendError> for StateMachineGone {
    fn from(_: mpsc::SendError) -> Self {
        StateMachineGone
    }
}

impl From<oneshot::Canceled> for StateMachineGone {
    fn from(_: oneshot::Canceled) -> Self {
        StateMachineGone
    }
}

enum ControlRequest {
    StartUpdateCheck { options: CheckOptions, responder: oneshot::Sender<StartUpdateCheckResponse> },
}

/// Responses to a request to start an update check now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartUpdateCheckResponse {
    /// The state machine was idle and the request triggered an update check.
    Started,

    /// The state machine was already processing an update check and ignored this request and
    /// options.
    AlreadyRunning,
}

impl ControlHandle {
    /// Ask the state machine to start an update check with the provided options, returning whether
    /// or not the state machine started a check or was already running one.
    pub async fn start_update_check(
        &mut self,
        options: CheckOptions,
    ) -> Result<StartUpdateCheckResponse, StateMachineGone> {
        let (responder, receive_response) = oneshot::channel();
        self.0.send(ControlRequest::StartUpdateCheck { options, responder }).await?;
        Ok(receive_response.await?)
    }
}

impl<PE, HR, IN, TM, MR, ST> StateMachine<PE, HR, IN, TM, MR, ST>
where
    PE: PolicyEngine,
    HR: HttpRequest,
    IN: Installer,
    TM: Timer,
    MR: MetricsReporter,
    ST: Storage,
{
    /// Need to do this in a mutable method because the borrow checker isn't smart enough to know
    /// that different fields of the same struct (even if it's not self) are separate variables and
    /// can be borrowed at the same time.
    async fn update_next_update_time(
        &mut self,
        co: &mut async_generator::Yield<StateMachineEvent>,
    ) -> CheckTiming {
        let apps = self.app_set.to_vec().await;
        let timing = self
            .policy_engine
            .compute_next_update_time(&apps, &self.context.schedule, &self.context.state)
            .await;
        self.context.schedule.next_update_time = Some(timing);

        co.yield_(StateMachineEvent::ScheduleChange(self.context.schedule.clone())).await;
        timing
    }

    async fn run(
        mut self,
        mut control: mpsc::Receiver<ControlRequest>,
        mut co: async_generator::Yield<StateMachineEvent>,
    ) {
        if !self.app_set.valid().await {
            error!(
                "App set not valid, not starting state machine: {:#?}",
                self.app_set.to_vec().await
            );
            return;
        }

        loop {
            info!("Initial context: {:?}", self.context);

            // Get the timing parameters for the next update check
            let check_timing: CheckTiming = self.update_next_update_time(&mut co).await;

            info!("Calculated check timing: {}", check_timing);

            let mut wait_to_next_check = if let Some(minimum_wait) = check_timing.minimum_wait {
                // If there's a minimum wait, also wait at least that long, by joining the two
                // timers so that both need to be true (in case `next_update_time` turns out to be
                // very close to now)
                future::join(
                    self.timer.wait_for(minimum_wait),
                    self.timer.wait_until(check_timing.time),
                )
                .map(|_| ())
                .boxed()
                .fuse()
            } else {
                // Otherwise just setup the timer for the waiting until the next time.  This is a
                // wait until either the monotonic or wall times have passed.
                self.timer.wait_until(check_timing.time).fuse()
            };

            // Wait for either the next check time or a request to start an update check.  Use the
            // default check options with the timed check, or those sent with a request.
            let options = select! {
                () = wait_to_next_check => CheckOptions::default(),
                ControlRequest::StartUpdateCheck{options, responder} = control.select_next_some() => {
                    let _ = responder.send(StartUpdateCheckResponse::Started);
                    options
                }
            };

            // "start" the update check itself (well, create the future that is the update check)
            let update_check = self.start_update_check(options, &mut co).fuse();
            futures::pin_mut!(update_check);

            // Wait for the update check to complete, handling any control requests that come in
            // during the check.
            loop {
                select! {
                    () = update_check => break,
                    ControlRequest::StartUpdateCheck{options, responder} = control.select_next_some() => {
                        let _ = responder.send(StartUpdateCheckResponse::AlreadyRunning);
                    }

                }
            }
        }
    }

    /// Report update check interval based on the last check time stored in storage.
    /// It will also persist the new last check time to storage.
    async fn report_check_interval(&mut self) {
        // Clone the Rc first to avoid borrowing self for the rest of the function.
        let storage_ref = self.storage_ref.clone();
        let mut storage = storage_ref.lock().await;
        let now = self.time_source.now();
        if let Some(last_check_time) = storage.get_int(LAST_CHECK_TIME).await {
            match now.wall_duration_since(micros_from_epoch_to_system_time(last_check_time)) {
                Ok(duration) => self.report_metrics(Metrics::UpdateCheckInterval(duration)),
                Err(e) => warn!("Last check time is in the future: {}", e),
            }
        }
        if let Err(e) =
            storage.set_option_int(LAST_CHECK_TIME, now.checked_to_micros_since_epoch()).await
        {
            error!("Unable to persist {}: {}", LAST_CHECK_TIME, e);
            return;
        }
        if let Err(e) = storage.commit().await {
            error!("Unable to commit persisted data: {}", e);
        }
    }

    /// Perform update check and handle the result, including updating the update check context
    /// and cohort.
    pub async fn start_update_check(
        &mut self,
        options: CheckOptions,
        co: &mut async_generator::Yield<StateMachineEvent>,
    ) {
        let apps = self.app_set.to_vec().await;
        let result = self.perform_update_check(&options, self.context.clone(), apps, co).await;
        match &result {
            Ok(result) => {
                info!("Update check result: {:?}", result);
                // Update check succeeded, update |last_update_time|.
                self.context.schedule.last_update_time = Some(self.time_source.now().into());

                // Update the service dictated poll interval (which is an Option<>, so doesn't
                // need to be tested for existence here).
                self.context.state.server_dictated_poll_interval =
                    result.server_dictated_poll_interval;

                // Increment |consecutive_failed_update_attempts| if any app failed to install,
                // otherwise reset it to 0.
                if result
                    .app_responses
                    .iter()
                    .any(|app| app.result == update_check::Action::InstallPlanExecutionError)
                {
                    self.context.state.consecutive_failed_update_attempts += 1;
                } else {
                    self.context.state.consecutive_failed_update_attempts = 0;
                }

                // Update check succeeded, reset |consecutive_failed_update_checks| to 0.
                self.context.state.consecutive_failed_update_checks = 0;

                self.app_set.update_from_omaha(&result.app_responses).await;

                self.report_attempts_to_succeed(true).await;

                // TODO: update consecutive_proxied_requests
            }
            Err(error) => {
                error!("Update check failed: {:?}", error);
                // Update check failed, increment |consecutive_failed_update_checks|.
                self.context.state.consecutive_failed_update_checks += 1;

                let failure_reason = match error {
                    UpdateCheckError::ResponseParser(_) | UpdateCheckError::InstallPlan(_) => {
                        // We talked to Omaha, update |last_update_time|.
                        self.context.schedule.last_update_time =
                            Some(self.time_source.now().into());

                        UpdateCheckFailureReason::Omaha
                    }
                    UpdateCheckError::Policy(_) => UpdateCheckFailureReason::Internal,
                    UpdateCheckError::OmahaRequest(request_error) => match request_error {
                        OmahaRequestError::Json(_) | OmahaRequestError::HttpBuilder(_) => {
                            UpdateCheckFailureReason::Internal
                        }
                        OmahaRequestError::Hyper(_) | OmahaRequestError::HttpStatus(_) => {
                            UpdateCheckFailureReason::Network
                        }
                    },
                };
                self.report_metrics(Metrics::UpdateCheckFailureReason(failure_reason));

                self.report_attempts_to_succeed(false).await;
            }
        }

        co.yield_(StateMachineEvent::ScheduleChange(self.context.schedule.clone())).await;
        co.yield_(StateMachineEvent::ProtocolStateChange(self.context.state.clone())).await;
        co.yield_(StateMachineEvent::UpdateCheckResult(result)).await;

        self.persist_data().await;

        // TODO: This is the last place we read self.state, we should see if we can find another
        // way to achieve this so that we can remove self.state entirely.
        if self.state == State::WaitingForReboot {
            while !self.policy_engine.reboot_allowed(&options).await {
                info!("Reboot not allowed at the moment, will try again in 30 minutes...");
                self.timer.wait_for(Duration::from_secs(30 * 60)).await;
            }
            info!("Rebooting the system at the end of a successful update");
            if let Err(e) = self.installer.perform_reboot().await {
                error!("Unable to reboot the system: {}", e);
            }
        }
        self.set_state(State::Idle, co).await;
    }

    /// Update `CONSECUTIVE_FAILED_UPDATE_CHECKS` in storage and report the metrics if `success`.
    /// Does not commit the change to storage.
    async fn report_attempts_to_succeed(&mut self, success: bool) {
        let storage_ref = self.storage_ref.clone();
        let mut storage = storage_ref.lock().await;
        let attempts = storage.get_int(CONSECUTIVE_FAILED_UPDATE_CHECKS).await.unwrap_or(0) + 1;
        if success {
            if let Err(e) = storage.remove(CONSECUTIVE_FAILED_UPDATE_CHECKS).await {
                error!("Unable to remove {}: {}", CONSECUTIVE_FAILED_UPDATE_CHECKS, e);
            }
            self.report_metrics(Metrics::AttemptsToSucceed(attempts as u64));
        } else {
            if let Err(e) = storage.set_int(CONSECUTIVE_FAILED_UPDATE_CHECKS, attempts).await {
                error!("Unable to persist {}: {}", CONSECUTIVE_FAILED_UPDATE_CHECKS, e);
            }
        }
    }

    /// Persist all necessary data to storage.
    async fn persist_data(&self) {
        let mut storage = self.storage_ref.lock().await;
        self.context.persist(&mut *storage).await;
        self.app_set.persist(&mut *storage).await;

        if let Err(e) = storage.commit().await {
            error!("Unable to commit persisted data: {}", e);
        }
    }

    /// This function constructs the chain of async futures needed to perform all of the async tasks
    /// that comprise an update check.
    async fn perform_update_check(
        &mut self,
        options: &CheckOptions,
        context: update_check::Context,
        apps: Vec<App>,
        co: &mut async_generator::Yield<StateMachineEvent>,
    ) -> Result<update_check::Response, UpdateCheckError> {
        // TODO: Move this check outside perform_update_check() so that FIDL server can know if
        // update check is throttled.
        info!("Checking to see if an update check is allowed at this time for {:?}", apps);
        let decision = self
            .policy_engine
            .update_check_allowed(&apps, &context.schedule, &context.state, &options)
            .await;

        info!("The update check decision is: {:?}", decision);

        let request_params = match decision {
            // Positive results, will continue with the update check process
            CheckDecision::Ok(rp) | CheckDecision::OkUpdateDeferred(rp) => rp,

            // Negative results, exit early
            CheckDecision::TooSoon => {
                info!("Too soon for update check, ending");
                // TODO: Report status
                return Err(UpdateCheckError::Policy(decision));
            }
            CheckDecision::ThrottledByPolicy => {
                info!("Update check has been throttled by the Policy, ending");
                // TODO: Report status
                return Err(UpdateCheckError::Policy(decision));
            }
            CheckDecision::DeniedByPolicy => {
                info!("Update check has ben denied by the Policy");
                // TODO: Report status
                return Err(UpdateCheckError::Policy(decision));
            }
        };

        self.set_state(State::CheckingForUpdates, co).await;

        self.report_check_interval().await;

        // Construct a request for the app(s).
        let mut request_builder = RequestBuilder::new(&self.config, &request_params);
        for app in &apps {
            request_builder = request_builder.add_update_check(app).add_ping(app);
        }

        let update_check_start_time = Instant::now();
        let mut omaha_request_attempt = 1;
        let max_omaha_request_attempts = 3;
        let (_parts, data) = loop {
            match Self::do_omaha_request(&mut self.http, &request_builder).await {
                Ok(res) => {
                    break res;
                }
                Err(OmahaRequestError::Json(e)) => {
                    error!("Unable to construct request body! {:?}", e);
                    self.set_state(State::ErrorCheckingForUpdate, co).await;
                    return Err(UpdateCheckError::OmahaRequest(e.into()));
                }
                Err(OmahaRequestError::HttpBuilder(e)) => {
                    error!("Unable to construct HTTP request! {:?}", e);
                    self.set_state(State::ErrorCheckingForUpdate, co).await;
                    return Err(UpdateCheckError::OmahaRequest(e.into()));
                }
                Err(OmahaRequestError::Hyper(e)) => {
                    warn!("Unable to contact Omaha: {:?}", e);
                    // Don't retry if the error was caused by user code, which means we weren't
                    // using the library correctly.
                    if omaha_request_attempt >= max_omaha_request_attempts || e.is_user() {
                        self.set_state(State::ErrorCheckingForUpdate, co).await;
                        return Err(UpdateCheckError::OmahaRequest(e.into()));
                    }
                }
                Err(OmahaRequestError::HttpStatus(e)) => {
                    warn!("Unable to contact Omaha: {:?}", e);
                    if omaha_request_attempt >= max_omaha_request_attempts {
                        self.set_state(State::ErrorCheckingForUpdate, co).await;
                        return Err(UpdateCheckError::OmahaRequest(e.into()));
                    }
                }
            }

            // TODO(41738): Move this to Policy.
            // Randomized exponential backoff of 1, 2, & 4 seconds, +/- 500ms.
            let backoff_time_secs = 1 << (omaha_request_attempt - 1);
            let backoff_time = randomize(backoff_time_secs * 1000, 1000);
            info!("Waiting {} ms before retrying...", backoff_time);
            self.timer.wait_for(Duration::from_millis(backoff_time)).await;

            omaha_request_attempt += 1;
        };

        self.report_metrics(Metrics::UpdateCheckResponseTime(update_check_start_time.elapsed()));
        self.report_metrics(Metrics::UpdateCheckRetries(omaha_request_attempt));

        let response = match Self::parse_omaha_response(&data) {
            Ok(res) => res,
            Err(err) => {
                warn!("Unable to parse Omaha response: {:?}", err);
                self.set_state(State::ErrorCheckingForUpdate, co).await;
                let event = Event {
                    event_type: EventType::UpdateComplete,
                    errorcode: Some(EventErrorCode::ParseResponse),
                    ..Event::default()
                };
                self.report_omaha_event(&request_params, event, &apps).await;
                return Err(UpdateCheckError::ResponseParser(err));
            }
        };

        info!("result: {:?}", response);

        co.yield_(StateMachineEvent::OmahaServerResponse(response.clone())).await;

        let statuses = Self::get_app_update_statuses(&response);
        for (app_id, status) in &statuses {
            // TODO:  Report or metric statuses other than 'no-update' and 'ok'
            info!("Omaha update check status: {} => {:?}", app_id, status);
        }

        let some_app_has_update = statuses.iter().any(|(_id, status)| **status == OmahaStatus::Ok);
        if !some_app_has_update {
            // A succesfull, no-update, check

            self.set_state(State::NoUpdateAvailable, co).await;
            Ok(Self::make_response(response, update_check::Action::NoUpdate))
        } else {
            info!(
                "At least one app has an update, proceeding to build and process an Install Plan"
            );
            let install_plan = match IN::InstallPlan::try_create_from(&request_params, &response) {
                Ok(plan) => plan,
                Err(e) => {
                    error!("Unable to construct install plan! {}", e);
                    // report_error emits InstallationError, need to emit InstallingUpdate first
                    self.set_state(State::InstallingUpdate, co).await;
                    self.report_error(
                        &request_params,
                        EventErrorCode::ConstructInstallPlan,
                        &apps,
                        co,
                    )
                    .await;
                    return Err(UpdateCheckError::InstallPlan(e.into()));
                }
            };

            info!("Validating Install Plan with Policy");
            let install_plan_decision = self.policy_engine.update_can_start(&install_plan).await;
            match install_plan_decision {
                UpdateDecision::Ok => {
                    info!("Proceeding with install plan.");
                }
                UpdateDecision::DeferredByPolicy => {
                    info!("Install plan was deferred by Policy.");
                    // Report "error" to Omaha (as this is an event that needs reporting as the
                    // install isn't starting immediately.
                    let event = Event {
                        event_type: EventType::UpdateComplete,
                        event_result: EventResult::UpdateDeferred,
                        ..Event::default()
                    };
                    self.report_omaha_event(&request_params, event, &apps).await;

                    self.set_state(State::InstallationDeferredByPolicy, co).await;
                    return Ok(Self::make_response(
                        response,
                        update_check::Action::DeferredByPolicy,
                    ));
                }
                UpdateDecision::DeniedByPolicy => {
                    warn!("Install plan was denied by Policy, see Policy logs for reasoning");
                    // report_error emits InstallationError, need to emit InstallingUpdate first
                    self.set_state(State::InstallingUpdate, co).await;
                    self.report_error(&request_params, EventErrorCode::DeniedByPolicy, &apps, co)
                        .await;
                    return Ok(Self::make_response(response, update_check::Action::DeniedByPolicy));
                }
            }

            self.set_state(State::InstallingUpdate, co).await;
            self.report_success_event(&request_params, EventType::UpdateDownloadStarted, &apps)
                .await;

            let install_plan_id = install_plan.id();
            let update_start_time = SystemTime::from(self.time_source.now());
            let update_first_seen_time =
                self.record_update_first_seen_time(&install_plan_id, update_start_time).await;

            let (send, mut recv) = mpsc::channel(0);
            let observer = StateMachineProgressObserver(send);
            let perform_install = async {
                let result = self.installer.perform_install(&install_plan, Some(&observer)).await;
                // Drop observer so that we can stop waiting for the next progress.
                drop(observer);
                result
            };
            let yield_progress = async {
                while let Some(progress) = recv.next().await {
                    co.yield_(StateMachineEvent::InstallProgressChange(progress)).await;
                }
            };

            let (install_result, ()) = future::join(perform_install, yield_progress).await;
            if let Err(e) = install_result {
                warn!("Installation failed: {}", e);
                self.report_error(&request_params, EventErrorCode::Installation, &apps, co).await;

                match SystemTime::from(self.time_source.now()).duration_since(update_start_time) {
                    Ok(duration) => self.report_metrics(Metrics::FailedUpdateDuration(duration)),
                    Err(e) => warn!("Update start time is in the future: {}", e),
                }
                return Ok(Self::make_response(
                    response,
                    update_check::Action::InstallPlanExecutionError,
                ));
            }

            self.report_success_event(&request_params, EventType::UpdateDownloadFinished, &apps)
                .await;

            // TODO: Verify downloaded update if needed.

            self.report_success_event(&request_params, EventType::UpdateComplete, &apps).await;

            let update_finish_time = SystemTime::from(self.time_source.now());
            match update_finish_time.duration_since(update_start_time) {
                Ok(duration) => self.report_metrics(Metrics::SuccessfulUpdateDuration(duration)),
                Err(e) => warn!("Update start time is in the future: {}", e),
            }
            match update_finish_time.duration_since(update_first_seen_time) {
                Ok(duration) => {
                    self.report_metrics(Metrics::SuccessfulUpdateFromFirstSeen(duration))
                }
                Err(e) => warn!("Update first seen time is in the future: {}", e),
            }

            self.set_state(State::WaitingForReboot, co).await;
            Ok(Self::make_response(response, update_check::Action::Updated))
        }
    }

    /// Set the current state to |InstallationError| and report the error event to Omaha.
    async fn report_error<'a>(
        &'a mut self,
        request_params: &'a RequestParams,
        errorcode: EventErrorCode,
        apps: &'a Vec<App>,
        co: &mut async_generator::Yield<StateMachineEvent>,
    ) {
        self.set_state(State::InstallationError, co).await;

        let event = Event {
            event_type: EventType::UpdateComplete,
            errorcode: Some(errorcode),
            ..Event::default()
        };
        self.report_omaha_event(&request_params, event, apps).await;
    }

    /// Report a successful event to Omaha, for example download started, download finished, etc.
    async fn report_success_event<'a>(
        &'a mut self,
        request_params: &'a RequestParams,
        event_type: EventType,
        apps: &'a Vec<App>,
    ) {
        let event = Event { event_type, event_result: EventResult::Success, ..Event::default() };
        self.report_omaha_event(&request_params, event, apps).await;
    }

    /// Report the given |event| to Omaha, errors occurred during reporting are logged but not
    /// acted on.
    async fn report_omaha_event<'a>(
        &'a mut self,
        request_params: &'a RequestParams,
        event: Event,
        apps: &'a Vec<App>,
    ) {
        let mut request_builder = RequestBuilder::new(&self.config, &request_params);
        for app in apps {
            request_builder = request_builder.add_event(app, &event);
        }
        if let Err(e) = Self::do_omaha_request(&mut self.http, &request_builder).await {
            warn!("Unable to report event to Omaha: {:?}", e);
        }
    }

    /// Make an http request to Omaha, and collect the response into an error or a blob of bytes
    /// that can be parsed.
    ///
    /// Given the http client and the request build, this makes the http request, and then coalesces
    /// the various errors into a single error type for easier error handling by the make process
    /// flow.
    ///
    /// This function also converts an HTTP error response into an Error, to divert those into the
    /// error handling paths instead of the Ok() path.
    async fn do_omaha_request<'a>(
        http: &'a mut HR,
        builder: &RequestBuilder<'a>,
    ) -> Result<(Parts, Vec<u8>), OmahaRequestError> {
        let (parts, body) = Self::make_request(http, builder.build()?).await?;
        if !parts.status.is_success() {
            // Convert HTTP failure responses into Errors.
            Err(OmahaRequestError::HttpStatus(parts.status))
        } else {
            // Pass successful responses to the caller.
            info!("Omaha HTTP response: {}", parts.status);
            Ok((parts, body))
        }
    }

    /// Make an http request and collect the response body into a Vec of bytes.
    ///
    /// Specifically, this takes the body of the response and concatenates it into a single Vec of
    /// bytes so that any errors in receiving it can be captured immediately, instead of needing to
    /// handle them as part of parsing the response body.
    async fn make_request(
        http_client: &mut HR,
        request: http::Request<hyper::Body>,
    ) -> Result<(Parts, Vec<u8>), hyper::Error> {
        info!("Making http request to: {}", request.uri());
        let res = http_client.request(request).await.map_err(|err| {
            warn!("Unable to perform request: {}", err);
            err
        })?;

        let (parts, body) = res.into_parts();
        let data = body.compat().try_concat().await?;

        Ok((parts, data.to_vec()))
    }

    /// This method takes the response bytes from Omaha, and converts them into a protocol::Response
    /// struct, returning all of the various errors that can occur in that process as a consolidated
    /// error enum.
    fn parse_omaha_response(data: &[u8]) -> Result<Response, ResponseParseError> {
        parse_json_response(&data).map_err(ResponseParseError::Json)
    }

    /// Utility to extract pairs of app id => omaha status response, to make it easier to ask
    /// questions about the response.
    fn get_app_update_statuses(response: &Response) -> Vec<(&str, &OmahaStatus)> {
        response
            .apps
            .iter()
            .filter_map(|app| match &app.update_check {
                None => None,
                Some(u) => Some((app.id.as_str(), &u.status)),
            })
            .collect()
    }

    /// Utility to take a set of protocol::response::Apps and then construct a response from the
    /// update check based on those app IDs.
    ///
    /// TODO: Change the Policy and Installer to return a set of results, one for each app ID, then
    ///       make this match that.
    fn make_response(
        response: protocol::response::Response,
        action: update_check::Action,
    ) -> update_check::Response {
        update_check::Response {
            app_responses: response
                .apps
                .iter()
                .map(|app| update_check::AppResponse {
                    app_id: app.id.clone(),
                    cohort: app.cohort.clone(),
                    user_counting: response.daystart.clone().into(),
                    result: action.clone(),
                })
                .collect(),
            server_dictated_poll_interval: None,
        }
    }

    /// Update the state internally and send it to the observer.
    async fn set_state(
        &mut self,
        state: State,
        co: &mut async_generator::Yield<StateMachineEvent>,
    ) {
        self.state = state.clone();
        co.yield_(StateMachineEvent::StateChange(state)).await;
    }

    fn report_metrics(&mut self, metrics: Metrics) {
        if let Err(err) = self.metrics_reporter.report_metrics(metrics) {
            warn!("Unable to report metrics: {:?}", err);
        }
    }

    async fn record_update_first_seen_time(
        &mut self,
        install_plan_id: &str,
        now: SystemTime,
    ) -> SystemTime {
        let mut storage = self.storage_ref.lock().await;
        let previous_id = storage.get_string(INSTALL_PLAN_ID).await;
        if let Some(previous_id) = previous_id {
            if previous_id == install_plan_id {
                return storage
                    .get_int(UPDATE_FIRST_SEEN_TIME)
                    .await
                    .map(micros_from_epoch_to_system_time)
                    .unwrap_or(now);
            }
        }
        // Update INSTALL_PLAN_ID and UPDATE_FIRST_SEEN_TIME for new update.
        if let Err(e) = storage.set_string(INSTALL_PLAN_ID, install_plan_id).await {
            error!("Unable to persist {}: {}", INSTALL_PLAN_ID, e);
            return now;
        }
        if let Err(e) = storage
            .set_option_int(UPDATE_FIRST_SEEN_TIME, checked_system_time_to_micros_from_epoch(now))
            .await
        {
            error!("Unable to persist {}: {}", UPDATE_FIRST_SEEN_TIME, e);
            let _ = storage.remove(INSTALL_PLAN_ID).await;
            return now;
        }
        if let Err(e) = storage.commit().await {
            error!("Unable to commit persisted data: {}", e);
        }
        now
    }
}

/// Return a random number in [n - range / 2, n - range / 2 + range).
fn randomize(n: u64, range: u64) -> u64 {
    n - range / 2 + rand::random::<u64>() % range
}

#[cfg(test)]
impl<PE, HR, IN, TM, MR, ST> StateMachine<PE, HR, IN, TM, MR, ST>
where
    PE: PolicyEngine,
    HR: HttpRequest,
    IN: Installer,
    TM: Timer,
    MR: MetricsReporter,
    ST: Storage,
{
    /// Run perform_update_check once, returning the update check result.
    pub async fn oneshot(&mut self) -> Result<update_check::Response, UpdateCheckError> {
        let options = CheckOptions::default();

        let context = update_check::Context {
            schedule: UpdateCheckSchedule::builder()
                .last_time(self.time_source.now() - Duration::new(500, 0))
                .next_timing(CheckTiming::builder().time(self.time_source.now()).build())
                .build(),
            state: ProtocolState::default(),
        };

        let apps = self.app_set.to_vec().await;

        async_generator::generate(move |mut co| async move {
            self.perform_update_check(&options, context, apps, &mut co).await
        })
        .into_complete()
        .await
    }

    /// Run start_upate_check once, discarding its states.
    pub async fn run_once(&mut self) {
        let options = CheckOptions::default();

        async_generator::generate(move |mut co| async move {
            self.start_update_check(options, &mut co).await;
        })
        .map(|_| ())
        .collect::<()>()
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::update_check::*;
    use super::*;
    use crate::{
        common::{
            App, CheckOptions, PersistedApp, ProtocolState, UpdateCheckSchedule, UserCounting,
        },
        http_request::mock::MockHttpRequest,
        installer::{
            stub::{StubInstallErrors, StubInstaller, StubPlan},
            ProgressObserver,
        },
        metrics::MockMetricsReporter,
        policy::{MockPolicyEngine, StubPolicyEngine},
        protocol::{response, Cohort},
        storage::MemStorage,
        time::{
            timers::{BlockingTimer, MockTimer, RequestedWait},
            MockTimeSource, PartialComplexTime,
        },
    };
    use anyhow::anyhow;
    use futures::executor::{block_on, LocalPool};
    use futures::future::BoxFuture;
    use futures::task::LocalSpawnExt;
    use log::info;
    use matches::assert_matches;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::cell::RefCell;
    use std::time::Duration;

    fn make_test_app_set() -> AppSet {
        AppSet::new(vec![App::new(
            "{00000000-0000-0000-0000-000000000001}",
            [1, 2, 3, 4],
            Cohort::new("stable-channel"),
        )])
    }

    // Assert that the last request made to |http| is equal to the request built by
    // |request_builder|.
    async fn assert_request<'a>(http: MockHttpRequest, request_builder: RequestBuilder<'a>) {
        let body = request_builder
            .build()
            .unwrap()
            .into_body()
            .compat()
            .try_concat()
            .await
            .unwrap()
            .to_vec();
        // Compare string instead of Vec<u8> for easier debugging.
        let body_str = String::from_utf8_lossy(&body);
        http.assert_body_str(&body_str).await;
    }

    #[test]
    fn run_simple_check_with_noupdate_result() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "noupdate"
                }
              }]
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            StateMachineBuilder::new_stub().http(http).oneshot().await.unwrap();

            info!("update check complete!");
        });
    }

    #[test]
    fn test_cohort_returned_with_noupdate_result() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "cohort": "1",
                "cohortname": "stable-channel",
                "updatecheck": {
                  "status": "noupdate"
                }
              }]
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let response = StateMachineBuilder::new_stub().http(http).oneshot().await.unwrap();
            assert_eq!("{00000000-0000-0000-0000-000000000001}", response.app_responses[0].app_id);
            assert_eq!(Some("1".into()), response.app_responses[0].cohort.id);
            assert_eq!(Some("stable-channel".into()), response.app_responses[0].cohort.name);
            assert_eq!(None, response.app_responses[0].cohort.hint);
        });
    }

    #[test]
    fn test_report_parse_response_error() {
        block_on(async {
            let http = MockHttpRequest::new(hyper::Response::new("invalid response".into()));

            let mut state_machine = StateMachineBuilder::new_stub().http(http).build().await;

            let response = state_machine.oneshot().await;
            assert_matches!(response, Err(UpdateCheckError::ResponseParser(_)));

            let request_params = RequestParams::default();
            let mut request_builder = RequestBuilder::new(&state_machine.config, &request_params);
            let event = Event {
                event_type: EventType::UpdateComplete,
                errorcode: Some(EventErrorCode::ParseResponse),
                ..Event::default()
            };
            let apps = state_machine.app_set.to_vec().await;
            request_builder = request_builder.add_event(&apps[0], &event);
            assert_request(state_machine.http, request_builder).await;
        });
    }

    #[test]
    fn test_report_construct_install_plan_error() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "4.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let mut state_machine = StateMachineBuilder::new_stub().http(http).build().await;

            let response = state_machine.oneshot().await;
            assert_matches!(response, Err(UpdateCheckError::InstallPlan(_)));

            let request_params = RequestParams::default();
            let mut request_builder = RequestBuilder::new(&state_machine.config, &request_params);
            let event = Event {
                event_type: EventType::UpdateComplete,
                errorcode: Some(EventErrorCode::ConstructInstallPlan),
                ..Event::default()
            };
            let apps = state_machine.app_set.to_vec().await;
            request_builder = request_builder.add_event(&apps[0], &event);
            assert_request(state_machine.http, request_builder).await;
        });
    }

    #[test]
    fn test_report_installation_error() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let mut state_machine = StateMachineBuilder::new_stub()
                .http(http)
                .installer(StubInstaller { should_fail: true })
                .build()
                .await;

            let response = state_machine.oneshot().await.unwrap();
            assert_eq!(Action::InstallPlanExecutionError, response.app_responses[0].result);

            let request_params = RequestParams::default();
            let mut request_builder = RequestBuilder::new(&state_machine.config, &request_params);
            let event = Event {
                event_type: EventType::UpdateComplete,
                errorcode: Some(EventErrorCode::Installation),
                ..Event::default()
            };
            let apps = state_machine.app_set.to_vec().await;
            request_builder = request_builder.add_event(&apps[0], &event);
            assert_request(state_machine.http, request_builder).await;
        });
    }

    #[test]
    fn test_report_deferred_by_policy() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let policy_engine = MockPolicyEngine {
                update_decision: UpdateDecision::DeferredByPolicy,
                ..MockPolicyEngine::default()
            };
            let mut state_machine = StateMachineBuilder::new_stub()
                .policy_engine(policy_engine)
                .http(http)
                .build()
                .await;

            let response = state_machine.oneshot().await.unwrap();
            assert_eq!(Action::DeferredByPolicy, response.app_responses[0].result);

            let request_params = RequestParams::default();
            let mut request_builder = RequestBuilder::new(&state_machine.config, &request_params);
            let event = Event {
                event_type: EventType::UpdateComplete,
                event_result: EventResult::UpdateDeferred,
                ..Event::default()
            };
            let apps = state_machine.app_set.to_vec().await;
            request_builder = request_builder.add_event(&apps[0], &event);
            assert_request(state_machine.http, request_builder).await;
        });
    }

    #[test]
    fn test_report_denied_by_policy() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let policy_engine = MockPolicyEngine {
                update_decision: UpdateDecision::DeniedByPolicy,
                ..MockPolicyEngine::default()
            };

            let mut state_machine = StateMachineBuilder::new_stub()
                .policy_engine(policy_engine)
                .http(http)
                .build()
                .await;

            let response = state_machine.oneshot().await.unwrap();
            assert_eq!(Action::DeniedByPolicy, response.app_responses[0].result);

            let request_params = RequestParams::default();
            let mut request_builder = RequestBuilder::new(&state_machine.config, &request_params);
            let event = Event {
                event_type: EventType::UpdateComplete,
                errorcode: Some(EventErrorCode::DeniedByPolicy),
                ..Event::default()
            };
            let apps = state_machine.app_set.to_vec().await;
            request_builder = request_builder.add_event(&apps[0], &event);
            assert_request(state_machine.http, request_builder).await;
        });
    }

    #[test]
    fn test_wait_timer() {
        let mut pool = LocalPool::new();
        let mock_time = MockTimeSource::new_from_now();
        let next_update_time = mock_time.now() + Duration::from_secs(111);
        let (timer, mut timers) = BlockingTimer::new();
        let policy_engine = MockPolicyEngine {
            check_timing: Some(CheckTiming::builder().time(next_update_time).build()),
            time_source: mock_time,
            ..MockPolicyEngine::default()
        };

        let (_ctl, state_machine) = pool.run_until(
            StateMachineBuilder::new_stub().policy_engine(policy_engine).timer(timer).start(),
        );

        pool.spawner().spawn_local(state_machine.map(|_| ()).collect()).unwrap();

        // With otherwise stub implementations, the pool stalls when a timer is awaited.  Dropping
        // the state machine will panic if any timer durations were not used.
        let blocked_timer = pool.run_until(timers.next()).unwrap();
        assert_eq!(blocked_timer.requested_wait(), RequestedWait::Until(next_update_time.into()));
    }

    #[test]
    fn test_cohort_and_user_counting_updates_are_used_in_subsequent_requests() {
        block_on(async {
            let response = json!({"response":{
                "server": "prod",
                "protocol": "3.0",
                "daystart": {
                  "elapsed_days": 1234567,
                  "elapsed_seconds": 3645
                },
                "app": [{
                  "appid": "{00000000-0000-0000-0000-000000000001}",
                  "status": "ok",
                  "cohort": "1",
                  "cohortname": "stable-channel",
                  "updatecheck": {
                    "status": "noupdate"
                  }
                }]
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let mut http = MockHttpRequest::new(hyper::Response::new(response.clone().into()));
            http.add_response(hyper::Response::new(response.into()));
            let last_request_viewer = MockHttpRequest::from_request_cell(http.get_request_cell());
            let apps = make_test_app_set();

            let mut state_machine =
                StateMachineBuilder::new_stub().http(http).app_set(apps.clone()).build().await;

            // Run it the first time.
            state_machine.run_once().await;

            let apps = apps.to_vec().await;
            assert_eq!(Some("1".to_string()), apps[0].cohort.id);
            assert_eq!(None, apps[0].cohort.hint);
            assert_eq!(Some("stable-channel".to_string()), apps[0].cohort.name);
            assert_eq!(UserCounting::ClientRegulatedByDate(Some(1234567)), apps[0].user_counting);

            // Run it the second time.
            state_machine.run_once().await;

            let request_params = RequestParams::default();
            let expected_request_builder =
                RequestBuilder::new(&state_machine.config, &request_params)
                    .add_update_check(&apps[0])
                    .add_ping(&apps[0]);
            // Check that the second update check used the new app.
            assert_request(last_request_viewer, expected_request_builder).await;
        });
    }

    #[test]
    fn test_user_counting_returned() {
        block_on(async {
            let response = json!({"response":{
            "server": "prod",
            "protocol": "3.0",
            "daystart": {
              "elapsed_days": 1234567,
              "elapsed_seconds": 3645
            },
            "app": [{
              "appid": "{00000000-0000-0000-0000-000000000001}",
              "status": "ok",
              "cohort": "1",
              "cohortname": "stable-channel",
              "updatecheck": {
                "status": "noupdate"
                  }
              }]
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let response = StateMachineBuilder::new_stub().http(http).oneshot().await.unwrap();

            assert_eq!(
                UserCounting::ClientRegulatedByDate(Some(1234567)),
                response.app_responses[0].user_counting
            );
        });
    }

    #[test]
    fn test_observe_state() {
        block_on(async {
            let actual_states = StateMachineBuilder::new_stub()
                .oneshot_check(CheckOptions::default())
                .await
                .filter_map(|event| {
                    future::ready(match event {
                        StateMachineEvent::StateChange(state) => Some(state),
                        _ => None,
                    })
                })
                .collect::<Vec<State>>()
                .await;

            let expected_states =
                vec![State::CheckingForUpdates, State::ErrorCheckingForUpdate, State::Idle];
            assert_eq!(actual_states, expected_states);
        });
    }

    #[test]
    fn test_observe_schedule() {
        block_on(async {
            let mock_time = MockTimeSource::new_from_now();
            let actual_schedules = StateMachineBuilder::new_stub()
                .policy_engine(StubPolicyEngine::new(&mock_time))
                .oneshot_check(CheckOptions::default())
                .await
                .filter_map(|event| {
                    future::ready(match event {
                        StateMachineEvent::ScheduleChange(schedule) => Some(schedule),
                        _ => None,
                    })
                })
                .collect::<Vec<UpdateCheckSchedule>>()
                .await;

            // The resultant schedule should only contain the timestamp of the above update check.
            let expected_schedule =
                UpdateCheckSchedule::builder().last_time(mock_time.now()).build();

            assert_eq!(actual_schedules, vec![expected_schedule]);
        });
    }

    #[test]
    fn test_observe_protocol_state() {
        block_on(async {
            let actual_protocol_states = StateMachineBuilder::new_stub()
                .oneshot_check(CheckOptions::default())
                .await
                .filter_map(|event| {
                    future::ready(match event {
                        StateMachineEvent::ProtocolStateChange(state) => Some(state),
                        _ => None,
                    })
                })
                .collect::<Vec<ProtocolState>>()
                .await;

            let expected_protocol_state =
                ProtocolState { consecutive_failed_update_checks: 1, ..ProtocolState::default() };

            assert_eq!(actual_protocol_states, vec![expected_protocol_state]);
        });
    }

    #[test]
    fn test_observe_omaha_server_response() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "cohort": "1",
                "cohortname": "stable-channel",
                "updatecheck": {
                  "status": "noupdate"
                }
              }]
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let expected_omaha_response = response::parse_json_response(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));

            let actual_omaha_response = StateMachineBuilder::new_stub()
                .http(http)
                .oneshot_check(CheckOptions::default())
                .await
                .filter_map(|event| {
                    future::ready(match event {
                        StateMachineEvent::OmahaServerResponse(response) => Some(response),
                        _ => None,
                    })
                })
                .collect::<Vec<response::Response>>()
                .await;

            assert_eq!(actual_omaha_response, vec![expected_omaha_response]);
        });
    }

    #[test]
    fn test_metrics_report_update_check_response_time() {
        block_on(async {
            let mut metrics_reporter = MockMetricsReporter::new();
            let _response = StateMachineBuilder::new_stub()
                .metrics_reporter(&mut metrics_reporter)
                .oneshot()
                .await;

            assert!(!metrics_reporter.metrics.is_empty());
            match &metrics_reporter.metrics[0] {
                Metrics::UpdateCheckResponseTime(_) => {} // expected
                metric => panic!("Unexpected metric {:?}", metric),
            }
        });
    }

    #[test]
    fn test_metrics_report_update_check_retries() {
        block_on(async {
            let mut metrics_reporter = MockMetricsReporter::new();
            let _response = StateMachineBuilder::new_stub()
                .metrics_reporter(&mut metrics_reporter)
                .oneshot()
                .await;

            assert!(metrics_reporter.metrics.contains(&Metrics::UpdateCheckRetries(1)));
        });
    }

    #[test]
    fn test_update_check_retries_backoff_with_mock_timer() {
        block_on(async {
            let mut timer = MockTimer::new();
            timer.expect_for_range(Duration::from_millis(500), Duration::from_millis(1500));
            timer.expect_for_range(Duration::from_millis(1500), Duration::from_millis(2500));
            let requested_waits = timer.get_requested_waits_view();
            let response = StateMachineBuilder::new_stub()
                .http(MockHttpRequest::empty())
                .timer(timer)
                .oneshot()
                .await;

            let waits = requested_waits.borrow();
            assert_eq!(waits.len(), 2);
            assert_matches!(
                waits[0],
                RequestedWait::For(d) if d >= Duration::from_millis(500) && d <= Duration::from_millis(1500)
            );
            assert_matches!(
                waits[1],
                RequestedWait::For(d) if d >= Duration::from_millis(1500) && d <= Duration::from_millis(2500)
            );

            assert_matches!(
                response,
                Err(UpdateCheckError::OmahaRequest(OmahaRequestError::HttpStatus(_)))
            );
        });
    }

    #[test]
    fn test_metrics_report_update_check_failure_reason_omaha() {
        block_on(async {
            let mut metrics_reporter = MockMetricsReporter::new();
            let mut state_machine = StateMachineBuilder::new_stub()
                .metrics_reporter(&mut metrics_reporter)
                .build()
                .await;

            state_machine.run_once().await;

            assert!(metrics_reporter
                .metrics
                .contains(&Metrics::UpdateCheckFailureReason(UpdateCheckFailureReason::Omaha)));
        });
    }

    #[test]
    fn test_metrics_report_update_check_failure_reason_network() {
        block_on(async {
            let mut metrics_reporter = MockMetricsReporter::new();
            let mut state_machine = StateMachineBuilder::new_stub()
                .http(MockHttpRequest::empty())
                .metrics_reporter(&mut metrics_reporter)
                .build()
                .await;

            state_machine.run_once().await;

            assert!(metrics_reporter
                .metrics
                .contains(&Metrics::UpdateCheckFailureReason(UpdateCheckFailureReason::Network)));
        });
    }

    #[test]
    fn test_persist_last_update_time() {
        block_on(async {
            let storage = Rc::new(Mutex::new(MemStorage::new()));

            StateMachineBuilder::new_stub()
                .storage(Rc::clone(&storage))
                .oneshot_check(CheckOptions::default())
                .await
                .map(|_| ())
                .collect::<()>()
                .await;

            let storage = storage.lock().await;
            storage.get_int(LAST_UPDATE_TIME).await.unwrap();
            assert_eq!(true, storage.committed());
        });
    }

    #[test]
    fn test_persist_server_dictated_poll_interval() {
        block_on(async {
            // TODO: update this test to have a mocked http response with server dictated poll
            // interval when out code support parsing it from the response.
            let storage = Rc::new(Mutex::new(MemStorage::new()));

            StateMachineBuilder::new_stub()
                .storage(Rc::clone(&storage))
                .oneshot_check(CheckOptions::default())
                .await
                .map(|_| ())
                .collect::<()>()
                .await;

            let storage = storage.lock().await;
            assert!(storage.get_int(SERVER_DICTATED_POLL_INTERVAL).await.is_none());
            assert!(storage.committed());
        });
    }

    #[test]
    fn test_persist_app() {
        block_on(async {
            let storage = Rc::new(Mutex::new(MemStorage::new()));
            let app_set = make_test_app_set();

            StateMachineBuilder::new_stub()
                .storage(Rc::clone(&storage))
                .app_set(app_set.clone())
                .oneshot_check(CheckOptions::default())
                .await
                .map(|_| ())
                .collect::<()>()
                .await;

            let storage = storage.lock().await;
            let apps = app_set.to_vec().await;
            storage.get_string(&apps[0].id).await.unwrap();
            assert!(storage.committed());
        });
    }

    #[test]
    fn test_load_last_update_time() {
        block_on(async {
            let mut storage = MemStorage::new();
            let mock_time = MockTimeSource::new_from_now();
            let last_update_time = mock_time.now() - Duration::from_secs(999);
            let stored_time = last_update_time.checked_to_micros_since_epoch().unwrap();
            // The expected time is to account for the loss of precision in the conversions.
            // (nanos to micros)
            let expected_time = PartialComplexTime::from_micros_since_epoch(stored_time);
            storage.set_int(LAST_UPDATE_TIME, stored_time).await.unwrap();

            let state_machine = StateMachineBuilder::new_stub()
                .policy_engine(StubPolicyEngine::new(&mock_time))
                .storage(Rc::new(Mutex::new(storage)))
                .build()
                .await;

            assert_eq!(state_machine.context.schedule.last_update_time.unwrap(), expected_time);
        });
    }

    #[test]
    fn test_load_server_dictated_poll_interval() {
        block_on(async {
            let mut storage = MemStorage::new();
            storage.set_int(SERVER_DICTATED_POLL_INTERVAL, 56789).await.unwrap();

            let state_machine =
                StateMachineBuilder::new_stub().storage(Rc::new(Mutex::new(storage))).build().await;

            assert_eq!(
                Some(Duration::from_micros(56789)),
                state_machine.context.state.server_dictated_poll_interval
            );
        });
    }

    #[test]
    fn test_load_app() {
        block_on(async {
            let app_set = AppSet::new(vec![App::new(
                "{00000000-0000-0000-0000-000000000001}",
                [1, 2, 3, 4],
                Cohort::default(),
            )]);
            let mut storage = MemStorage::new();
            let persisted_app = PersistedApp {
                cohort: Cohort {
                    id: Some("cohort_id".to_string()),
                    hint: Some("test_channel".to_string()),
                    name: None,
                },
                user_counting: UserCounting::ClientRegulatedByDate(Some(22222)),
            };
            let json = serde_json::to_string(&persisted_app).unwrap();
            let apps = app_set.to_vec().await;
            storage.set_string(&apps[0].id, &json).await.unwrap();

            let _state_machine = StateMachineBuilder::new_stub()
                .storage(Rc::new(Mutex::new(storage)))
                .app_set(app_set.clone())
                .build()
                .await;

            let apps = app_set.to_vec().await;
            assert_eq!(persisted_app.cohort, apps[0].cohort);
            assert_eq!(UserCounting::ClientRegulatedByDate(Some(22222)), apps[0].user_counting);
        });
    }

    #[test]
    fn test_report_check_interval() {
        block_on(async {
            let mut mock_time = MockTimeSource::new_from_now();
            // Conversion from ComplexTime to i64 microseconds, necessary for a change in precision
            // as the Time structs have nanosecond precision.
            let start_time = mock_time.now();
            let start_time_i64_micros = start_time.checked_to_micros_since_epoch().unwrap();
            let mut state_machine = StateMachineBuilder::new_stub()
                .policy_engine(StubPolicyEngine::new(mock_time.clone()))
                .metrics_reporter(MockMetricsReporter::new())
                .storage(Rc::new(Mutex::new(MemStorage::new())))
                .build()
                .await;

            state_machine.report_check_interval().await;
            // No metrics should be reported because no last check time in storage.
            assert!(state_machine.metrics_reporter.metrics.is_empty());
            {
                let storage = state_machine.storage_ref.lock().await;
                assert_eq!(storage.get_int(LAST_CHECK_TIME).await.unwrap(), start_time_i64_micros);
                assert_eq!(storage.len(), 1);
                assert!(storage.committed());
            }
            // A second update check should report metrics.
            let duration = Duration::from_micros(999999);
            mock_time.advance(duration);

            // The calculated duration will have components smaller than 1ms, which need to be
            // accounted for (since the time isn't ever at 0ns)
            let later_time = mock_time.now();
            let later_time_i64_micros = later_time.checked_to_micros_since_epoch().unwrap();
            let start_time_from_i64 = micros_from_epoch_to_system_time(start_time_i64_micros);
            let calculated_duration = later_time.wall_duration_since(start_time_from_i64).unwrap();

            state_machine.report_check_interval().await;

            assert_eq!(
                state_machine.metrics_reporter.metrics,
                vec![Metrics::UpdateCheckInterval(calculated_duration)]
            );
            let storage = state_machine.storage_ref.lock().await;
            assert_eq!(storage.get_int(LAST_CHECK_TIME).await.unwrap(), later_time_i64_micros);
            assert_eq!(storage.len(), 1);
            assert!(storage.committed());
        });
    }

    #[derive(Debug)]
    pub struct TestInstaller {
        reboot_called: bool,
        should_fail: bool,
        mock_time: MockTimeSource,
    }
    struct TestInstallerBuilder {
        should_fail: Option<bool>,
        mock_time: MockTimeSource,
    }
    impl TestInstaller {
        fn builder(mock_time: MockTimeSource) -> TestInstallerBuilder {
            TestInstallerBuilder { should_fail: None, mock_time }
        }
    }
    impl TestInstallerBuilder {
        fn should_fail(mut self, should_fail: bool) -> Self {
            self.should_fail = Some(should_fail);
            self
        }
        fn build(self) -> TestInstaller {
            TestInstaller {
                reboot_called: false,
                should_fail: self.should_fail.unwrap_or(false),
                mock_time: self.mock_time,
            }
        }
    }
    const INSTALL_DURATION: Duration = Duration::from_micros(98765433);

    impl Installer for TestInstaller {
        type InstallPlan = StubPlan;
        type Error = StubInstallErrors;
        fn perform_install<'a>(
            &'a mut self,
            _install_plan: &StubPlan,
            observer: Option<&'a dyn ProgressObserver>,
        ) -> BoxFuture<'a, Result<(), Self::Error>> {
            if self.should_fail {
                future::ready(Err(StubInstallErrors::Failed)).boxed()
            } else {
                self.mock_time.advance(INSTALL_DURATION);
                async move {
                    if let Some(observer) = observer {
                        observer.receive_progress(None, 0.0, None, None).await;
                        observer.receive_progress(None, 0.3, None, None).await;
                        observer.receive_progress(None, 0.9, None, None).await;
                        observer.receive_progress(None, 1.0, None, None).await;
                    }
                    Ok(())
                }
                .boxed()
            }
        }

        fn perform_reboot(&mut self) -> BoxFuture<'_, Result<(), anyhow::Error>> {
            self.reboot_called = true;
            if self.should_fail {
                future::ready(Err(anyhow!("reboot failed"))).boxed()
            } else {
                future::ready(Ok(())).boxed()
            }
        }
    }

    #[test]
    fn test_report_successful_update_duration() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let storage = Rc::new(Mutex::new(MemStorage::new()));

            let mock_time = MockTimeSource::new_from_now();
            let now = mock_time.now();

            let update_completed_time = now + INSTALL_DURATION;
            let expected_update_duration = update_completed_time.wall_duration_since(now).unwrap();

            let first_seen_time = now - Duration::from_micros(100000000);
            let first_seen_time_i64_micros =
                first_seen_time.checked_to_micros_since_epoch().unwrap();
            let first_seen_time_from_i64 =
                micros_from_epoch_to_system_time(first_seen_time_i64_micros);

            let expected_duration_since_first_seen =
                update_completed_time.wall_duration_since(first_seen_time_from_i64).unwrap();

            let mut state_machine = StateMachineBuilder::new_stub()
                .http(http)
                .installer(TestInstaller::builder(mock_time.clone()).build())
                .policy_engine(StubPolicyEngine::new(mock_time.clone()))
                .metrics_reporter(MockMetricsReporter::new())
                .storage(Rc::clone(&storage))
                .build()
                .await;

            {
                let mut storage = storage.lock().await;
                storage.set_string(INSTALL_PLAN_ID, "").await.unwrap();
                storage.set_int(UPDATE_FIRST_SEEN_TIME, first_seen_time_i64_micros).await.unwrap();
                storage.commit().await.unwrap();
            }

            state_machine.run_once().await;

            let reported_metrics = state_machine.metrics_reporter.metrics;
            assert_eq!(
                reported_metrics
                    .iter()
                    .filter(|m| match m {
                        Metrics::SuccessfulUpdateDuration(_) => true,
                        _ => false,
                    })
                    .collect::<Vec<_>>(),
                vec![&Metrics::SuccessfulUpdateDuration(expected_update_duration)]
            );
            assert_eq!(
                reported_metrics
                    .iter()
                    .filter(|m| match m {
                        Metrics::SuccessfulUpdateFromFirstSeen(_) => true,
                        _ => false,
                    })
                    .collect::<Vec<_>>(),
                vec![&Metrics::SuccessfulUpdateFromFirstSeen(expected_duration_since_first_seen)]
            );
        });
    }

    #[test]
    fn test_report_failed_update_duration() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let mut state_machine = StateMachineBuilder::new_stub()
                .http(http)
                .installer(StubInstaller { should_fail: true })
                .metrics_reporter(MockMetricsReporter::new())
                .build()
                .await;
            // clock::mock::set(time::i64_to_time(123456789));

            state_machine.run_once().await;

            assert!(state_machine
                .metrics_reporter
                .metrics
                .contains(&Metrics::FailedUpdateDuration(Duration::from_micros(0))));
        });
    }

    #[test]
    fn test_record_update_first_seen_time() {
        block_on(async {
            let storage = Rc::new(Mutex::new(MemStorage::new()));
            let mut state_machine =
                StateMachineBuilder::new_stub().storage(Rc::clone(&storage)).build().await;

            let now = micros_from_epoch_to_system_time(123456789);
            assert_eq!(state_machine.record_update_first_seen_time("id", now).await, now);
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_string(INSTALL_PLAN_ID).await, Some("id".to_string()));
                assert_eq!(storage.get_int(UPDATE_FIRST_SEEN_TIME).await, Some(123456789));
                assert_eq!(storage.len(), 2);
                assert!(storage.committed());
            }

            let now2 = now + Duration::from_secs(1000);
            assert_eq!(state_machine.record_update_first_seen_time("id", now2).await, now);
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_string(INSTALL_PLAN_ID).await, Some("id".to_string()));
                assert_eq!(storage.get_int(UPDATE_FIRST_SEEN_TIME).await, Some(123456789));
                assert_eq!(storage.len(), 2);
                assert!(storage.committed());
            }
            assert_eq!(state_machine.record_update_first_seen_time("id2", now2).await, now2);
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_string(INSTALL_PLAN_ID).await, Some("id2".to_string()));
                assert_eq!(storage.get_int(UPDATE_FIRST_SEEN_TIME).await, Some(1123456789));
                assert_eq!(storage.len(), 2);
                assert!(storage.committed());
            }
        });
    }

    #[test]
    fn test_report_attempts_to_succeed() {
        block_on(async {
            let storage = Rc::new(Mutex::new(MemStorage::new()));
            let mut state_machine = StateMachineBuilder::new_stub()
                .installer(StubInstaller { should_fail: true })
                .metrics_reporter(MockMetricsReporter::new())
                .storage(Rc::clone(&storage))
                .build()
                .await;

            state_machine.report_attempts_to_succeed(true).await;
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_int(CONSECUTIVE_FAILED_UPDATE_CHECKS).await, None);
                assert_eq!(storage.len(), 0);
            }
            assert_eq!(state_machine.metrics_reporter.metrics, vec![Metrics::AttemptsToSucceed(1)]);

            state_machine.report_attempts_to_succeed(false).await;
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_int(CONSECUTIVE_FAILED_UPDATE_CHECKS).await, Some(1));
                assert_eq!(storage.len(), 1);
            }

            state_machine.report_attempts_to_succeed(false).await;
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_int(CONSECUTIVE_FAILED_UPDATE_CHECKS).await, Some(2));
                assert_eq!(storage.len(), 1);
            }

            state_machine.report_attempts_to_succeed(true).await;
            {
                let storage = storage.lock().await;
                assert_eq!(storage.get_int(CONSECUTIVE_FAILED_UPDATE_CHECKS).await, None);
                assert_eq!(storage.len(), 0);
            }
            assert_eq!(
                state_machine.metrics_reporter.metrics,
                vec![Metrics::AttemptsToSucceed(1), Metrics::AttemptsToSucceed(3)]
            );
        });
    }

    #[test]
    fn test_successful_update_triggers_reboot() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let mock_time = MockTimeSource::new_from_now();
            let mut state_machine = StateMachineBuilder::new_stub()
                .http(http)
                .installer(TestInstaller::builder(mock_time.clone()).build())
                .policy_engine(StubPolicyEngine::new(mock_time))
                .build()
                .await;

            state_machine.run_once().await;

            assert!(state_machine.installer.reboot_called);
        });
    }

    #[test]
    fn test_failed_update_does_not_trigger_reboot() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let mock_time = MockTimeSource::new_from_now();
            let mut state_machine = StateMachineBuilder::new_stub()
                .http(http)
                .installer(TestInstaller::builder(mock_time.clone()).should_fail(true).build())
                .policy_engine(StubPolicyEngine::new(mock_time))
                .build()
                .await;

            state_machine.run_once().await;

            assert!(!state_machine.installer.reboot_called);
        });
    }

    #[test]
    fn test_reboot_not_allowed() {
        let mut pool = LocalPool::new();

        let response = json!({"response":{
          "server": "prod",
          "protocol": "3.0",
          "app": [{
            "appid": "{00000000-0000-0000-0000-000000000001}",
            "status": "ok",
            "updatecheck": {
              "status": "ok"
            }
          }],
        }});
        let response = serde_json::to_vec(&response).unwrap();
        let http = MockHttpRequest::new(hyper::Response::new(response.into()));
        let mock_time = MockTimeSource::new_from_now();
        let (timer, mut timers) = BlockingTimer::new();
        let policy_engine = MockPolicyEngine {
            time_source: mock_time.clone(),
            reboot_allowed: false,
            ..MockPolicyEngine::default()
        };

        let mut state_machine = pool.run_until(
            StateMachineBuilder::new_stub()
                .http(http)
                .installer(TestInstaller::builder(mock_time).build())
                .policy_engine(policy_engine)
                .timer(timer)
                .build(),
        );
        {
            let run_once = state_machine.run_once();
            futures::pin_mut!(run_once);

            match pool.run_until(future::select(run_once, timers.next())) {
                future::Either::Left(((), _)) => {
                    panic!("state_machine finished without waiting on timer")
                }
                future::Either::Right((blocked_timer, _)) => {
                    assert_eq!(
                        blocked_timer.unwrap().requested_wait(),
                        RequestedWait::For(Duration::from_secs(30 * 60))
                    );
                }
            }
        }
        assert!(!state_machine.installer.reboot_called);
    }

    #[derive(Debug)]
    struct BlockingInstaller {
        on_install: mpsc::Sender<oneshot::Sender<Result<(), StubInstallErrors>>>,
    }

    impl Installer for BlockingInstaller {
        type InstallPlan = StubPlan;
        type Error = StubInstallErrors;

        fn perform_install(
            &mut self,
            _install_plan: &StubPlan,
            _observer: Option<&dyn ProgressObserver>,
        ) -> BoxFuture<'_, Result<(), StubInstallErrors>> {
            let (send, recv) = oneshot::channel();
            let send_fut = self.on_install.send(send);

            async move {
                send_fut.await.unwrap();
                recv.await.unwrap()
            }
            .boxed()
        }

        fn perform_reboot(&mut self) -> BoxFuture<'_, Result<(), anyhow::Error>> {
            future::ready(Ok(())).boxed()
        }
    }

    #[derive(Debug, Default)]
    struct TestObserver {
        states: Rc<RefCell<Vec<State>>>,
    }

    impl TestObserver {
        fn observe(&self, s: impl Stream<Item = StateMachineEvent>) -> impl Future<Output = ()> {
            let states = Rc::clone(&self.states);
            async move {
                futures::pin_mut!(s);
                while let Some(event) = s.next().await {
                    match event {
                        StateMachineEvent::StateChange(state) => {
                            states.borrow_mut().push(state);
                        }
                        _ => {}
                    }
                }
            }
        }

        fn observe_until_terminal(
            &self,
            s: impl Stream<Item = StateMachineEvent>,
        ) -> impl Future<Output = ()> {
            let states = Rc::clone(&self.states);
            async move {
                futures::pin_mut!(s);
                while let Some(event) = s.next().await {
                    match event {
                        StateMachineEvent::StateChange(state) => {
                            states.borrow_mut().push(state);
                            match state {
                                State::Idle | State::WaitingForReboot => return,
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        fn take_states(&self) -> Vec<State> {
            std::mem::replace(&mut *self.states.borrow_mut(), vec![])
        }
    }

    #[test]
    fn test_start_update_during_update_replies_with_in_progress() {
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let response = json!({"response":{
          "server": "prod",
          "protocol": "3.0",
          "app": [{
            "appid": "{00000000-0000-0000-0000-000000000001}",
            "status": "ok",
            "updatecheck": {
              "status": "ok"
            }
          }],
        }});
        let response = serde_json::to_vec(&response).unwrap();
        let http = MockHttpRequest::new(hyper::Response::new(response.into()));
        let (send_install, mut recv_install) = mpsc::channel(0);
        let (mut ctl, state_machine) = pool.run_until(
            StateMachineBuilder::new_stub()
                .http(http)
                .installer(BlockingInstaller { on_install: send_install })
                .start(),
        );

        let observer = TestObserver::default();
        spawner.spawn_local(observer.observe_until_terminal(state_machine)).unwrap();

        let unblock_install = pool.run_until(recv_install.next()).unwrap();
        pool.run_until_stalled();
        assert_eq!(
            observer.take_states(),
            vec![State::CheckingForUpdates, State::InstallingUpdate]
        );

        pool.run_until(async {
            assert_eq!(
                ctl.start_update_check(CheckOptions::default()).await,
                Ok(StartUpdateCheckResponse::AlreadyRunning)
            );
        });
        pool.run_until_stalled();
        assert_eq!(observer.take_states(), vec![]);

        unblock_install.send(Ok(())).unwrap();
        pool.run_until_stalled();

        assert_eq!(observer.take_states(), vec![State::WaitingForReboot]);
    }

    #[test]
    fn test_start_update_during_timer_starts_update() {
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let mut mock_time = MockTimeSource::new_from_now();
        let next_update_time = mock_time.now() + Duration::from_secs(321);

        let (timer, mut timers) = BlockingTimer::new();
        let policy_engine = MockPolicyEngine {
            check_timing: Some(CheckTiming::builder().time(next_update_time).build()),
            time_source: mock_time.clone(),
            ..MockPolicyEngine::default()
        };
        let (mut ctl, state_machine) = pool.run_until(
            StateMachineBuilder::new_stub().policy_engine(policy_engine).timer(timer).start(),
        );

        let observer = TestObserver::default();
        spawner.spawn_local(observer.observe(state_machine)).unwrap();

        let blocked_timer = pool.run_until(timers.next()).unwrap();
        assert_eq!(blocked_timer.requested_wait(), RequestedWait::Until(next_update_time.into()));
        mock_time.advance(Duration::from_secs(200));
        assert_eq!(observer.take_states(), vec![]);

        // Nothing happens while the timer is waiting.
        pool.run_until_stalled();
        assert_eq!(observer.take_states(), vec![]);

        blocked_timer.unblock();
        let blocked_timer = pool.run_until(timers.next()).unwrap();
        assert_eq!(blocked_timer.requested_wait(), RequestedWait::Until(next_update_time.into()));
        assert_eq!(
            observer.take_states(),
            vec![State::CheckingForUpdates, State::ErrorCheckingForUpdate, State::Idle]
        );

        // Unless a control signal to start an update check comes in.
        pool.run_until(async {
            assert_eq!(
                ctl.start_update_check(CheckOptions::default()).await,
                Ok(StartUpdateCheckResponse::Started)
            );
        });
        pool.run_until_stalled();
        assert_eq!(
            observer.take_states(),
            vec![State::CheckingForUpdates, State::ErrorCheckingForUpdate, State::Idle]
        );
    }

    #[test]
    fn test_progress_observer() {
        block_on(async {
            let response = json!({"response":{
              "server": "prod",
              "protocol": "3.0",
              "app": [{
                "appid": "{00000000-0000-0000-0000-000000000001}",
                "status": "ok",
                "updatecheck": {
                  "status": "ok"
                }
              }],
            }});
            let response = serde_json::to_vec(&response).unwrap();
            let http = MockHttpRequest::new(hyper::Response::new(response.into()));
            let mock_time = MockTimeSource::new_from_now();
            let progresses = StateMachineBuilder::new_stub()
                .http(http)
                .installer(TestInstaller::builder(mock_time.clone()).build())
                .policy_engine(StubPolicyEngine::new(mock_time))
                .oneshot_check(CheckOptions::default())
                .await
                .filter_map(|event| {
                    future::ready(match event {
                        StateMachineEvent::InstallProgressChange(InstallProgress { progress }) => {
                            Some(progress)
                        }
                        _ => None,
                    })
                })
                .collect::<Vec<f32>>()
                .await;
            assert_eq!(progresses, [0.0, 0.3, 0.9, 1.0]);
        });
    }
}
