//! Pure request decision logic for one supervisor state machine.
//!
//! This module validates a decoded request against service identity,
//! optimistic generation guards, and current lifecycle state. It mutates only
//! operator intent inside `StateMachine` and returns a `ControlEffect` for the
//! process executor to perform later; no socket, signal, or process I/O occurs
//! here, so malformed or conflicting requests are rejected before side effects.

use crate::{
    status::StatusSnapshot,
    supervisor::{DesiredState, Generation, StateMachine, SupervisorState},
};

use super::{Operation, Request, Response, ResponseCode, Signal, SignalScope, message};

/// Action the process executor must complete after accepting a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlEffect {
    /// Status or an idempotent lifecycle request needs no executor work.
    None,
    /// Begin dependency/condition evaluation and then create a generation.
    BeginStart,
    /// Cancel a pending condition or backoff, optionally starting immediately.
    CancelPending {
        /// Start again after cancellation.
        start: bool,
    },
    /// Stop and reap the owned process group, then apply the completion action.
    StopGroup {
        /// Exact generation to stop.
        generation: Generation,
        /// Desired action after reaping.
        after: StopCompletion,
    },
    /// Exit the supervisor, optionally leaving a live child deliberately orphaned.
    ExitSupervisor {
        /// Whether the current child/process group must remain running.
        leave_child: bool,
    },
    /// Deliver a raw signal without changing desired state.
    DeliverSignal {
        /// Exact live generation checked by the request.
        generation: Generation,
        /// Main process or owned group target.
        scope: SignalScope,
        /// Signal to deliver.
        signal: Signal,
    },
}

/// Lifecycle action after a stopped group has been fully reaped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCompletion {
    /// Remain supervised Down.
    Down,
    /// Begin a replacement generation.
    Restart,
    /// Exit the supervisor after cleanup.
    Halt,
}

/// Validated response plus executor work selected for a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlDecision {
    /// Immediate acceptance or rejection response.
    pub response: Response,
    /// Work which must complete for an accepted mutation.
    pub effect: ControlEffect,
}

/// Validate a request against one supervisor and update operator intent.
///
/// This function never performs process or socket I/O. The caller must execute
/// [`ControlDecision::effect`] and publish completion before treating an
/// accepted lifecycle operation as complete.
pub fn decide_request(
    service_name: &str,
    machine: &mut StateMachine,
    request: &Request,
) -> ControlDecision {
    if request.service != service_name {
        return rejected(machine, ResponseCode::NotFound, "service name mismatch");
    }
    if let Err(error) = message::validate_request(request) {
        return rejected(machine, ResponseCode::Invalid, &error.to_string());
    }
    let generation = machine.state().generation();
    if !request.expected_generation.matches(generation) {
        return rejected(
            machine,
            ResponseCode::Conflict,
            "service generation changed",
        );
    }

    let effect = match request.operation {
        Operation::Status => return status_decision(machine, generation),
        Operation::Start => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Up);
            start_effect(machine.state())
        }
        Operation::Once => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Once);
            start_effect(machine.state())
        }
        Operation::Stop => {
            machine.set_desired(DesiredState::Down);
            stop_effect(machine.state(), StopCompletion::Down)
        }
        Operation::Restart => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Up);
            machine.state().live_generation().map_or_else(
                || start_effect(machine.state()),
                |generation| ControlEffect::StopGroup {
                    generation,
                    after: StopCompletion::Restart,
                },
            )
        }
        Operation::Halt => {
            machine.set_desired(DesiredState::Halt);
            machine.state().live_generation().map_or(
                ControlEffect::ExitSupervisor { leave_child: false },
                |generation| ControlEffect::StopGroup {
                    generation,
                    after: StopCompletion::Halt,
                },
            )
        }
        Operation::Exit => {
            machine.set_desired(DesiredState::Exit);
            ControlEffect::ExitSupervisor {
                leave_child: machine.state().live_generation().is_some(),
            }
        }
        Operation::Signal => {
            let Some(generation) = machine.state().live_generation() else {
                return rejected(machine, ResponseCode::Invalid, "service has no live child");
            };
            let Some(signal) = request.signal else {
                return rejected(machine, ResponseCode::Invalid, "signal is missing");
            };
            ControlEffect::DeliverSignal {
                generation,
                scope: request.scope,
                signal,
            }
        }
    };
    ControlDecision {
        response: Response {
            code: ResponseCode::Ok,
            generation: machine.state().generation(),
            message: "accepted; awaiting lifecycle completion".to_owned(),
            status: None,
        },
        effect,
    }
}

fn status_decision(machine: &StateMachine, generation: Option<Generation>) -> ControlDecision {
    ControlDecision {
        response: Response {
            code: ResponseCode::Ok,
            generation,
            message: status_message(machine),
            status: Some(StatusSnapshot::from_machine(machine)),
        },
        effect: ControlEffect::None,
    }
}

fn start_effect(state: SupervisorState) -> ControlEffect {
    match state {
        SupervisorState::Down | SupervisorState::Failed(_) => ControlEffect::BeginStart,
        SupervisorState::WaitingCondition | SupervisorState::Backoff { .. } => {
            ControlEffect::CancelPending { start: true }
        }
        SupervisorState::Starting(_)
        | SupervisorState::Running(_)
        | SupervisorState::Ready(_)
        | SupervisorState::Paused { .. }
        | SupervisorState::Stopping(_)
        | SupervisorState::Completed(_)
        | SupervisorState::Initializing
        | SupervisorState::Exited => ControlEffect::None,
    }
}

fn stop_effect(state: SupervisorState, after: StopCompletion) -> ControlEffect {
    state.live_generation().map_or_else(
        || {
            if matches!(
                state,
                SupervisorState::WaitingCondition | SupervisorState::Backoff { .. }
            ) {
                ControlEffect::CancelPending { start: false }
            } else {
                ControlEffect::None
            }
        },
        |generation| ControlEffect::StopGroup { generation, after },
    )
}

fn rejected(machine: &StateMachine, code: ResponseCode, message: &str) -> ControlDecision {
    ControlDecision {
        response: Response {
            code,
            generation: machine.state().generation(),
            message: message.to_owned(),
            status: None,
        },
        effect: ControlEffect::None,
    }
}

fn status_message(machine: &StateMachine) -> String {
    format!(
        "desired={} state={}",
        desired_name(machine.desired()),
        state_name(machine.state())
    )
}

const fn desired_name(desired: DesiredState) -> &'static str {
    match desired {
        DesiredState::Up => "up",
        DesiredState::Down => "down",
        DesiredState::Once => "once",
        DesiredState::Halt => "halt",
        DesiredState::Exit => "exit",
    }
}

const fn state_name(state: SupervisorState) -> &'static str {
    match state {
        SupervisorState::Initializing => "initializing",
        SupervisorState::Down => "down",
        SupervisorState::WaitingCondition => "waiting-condition",
        SupervisorState::Starting(_) => "starting",
        SupervisorState::Running(_) => "running",
        SupervisorState::Ready(_) => "ready",
        SupervisorState::Paused { .. } => "paused",
        SupervisorState::Stopping(_) => "stopping",
        SupervisorState::Backoff { .. } => "backoff",
        SupervisorState::Completed(_) => "completed",
        SupervisorState::Failed(_) => "failed",
        SupervisorState::Exited => "exited",
    }
}
