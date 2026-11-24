//! Routines on the wire (13.19+, cap [`CAP_ROUTINES`]): saved instructions byob carries out
//! by itself (see [`byob_types::routine`]).
//!
//! The engine sends [`crate::ServerMsg::Routines`] after the Welcome and whenever a routine
//! or its state changes, [`crate::ServerMsg::RoutineRuns`] on request, and
//! [`crate::ServerMsg::RoutineEvent`] when a run reports, waits for a person or fails (the
//! desktop turns those into notifications). Schedules come described in words by the engine,
//! since only it reads rules.

use serde::{Deserialize, Serialize};

pub use byob_types::routine::{MAX_ID_CHARS, MAX_ROUTINES, Missed, Report, Routine, RunCause, RunRecord, RunStatus, Tell, Trigger, id_ok};

use crate::ClientMsg;

/// Cap: the peer handles routine messages.
pub const CAP_ROUTINES: &str = "routines";

/// One routine as clients see it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutineView {
    pub routine: Routine,
    /// Its schedules in words (`Every day at 8:00 AM`), one per schedule trigger; a schedule
    /// the engine can't read says why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<String>,
    /// When it's next due (ms since the epoch), if it's enabled and scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<i64>,
    /// About how many scheduled runs a month.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_per_month: Option<u32>,
    /// Its latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<RunRecord>,
}

/// What a [`crate::ServerMsg::RoutineEvent`] is about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineEventKind {
    /// A run called `report` (or, for `tell: always`, finished).
    #[default]
    Reported,
    /// A run waits for a person to approve a call.
    Waiting,
    /// A run failed.
    Failed,
    /// A kind a newer engine sends.
    #[serde(other)]
    Other,
}

/// Size checks of the routine messages (the engine checks the rest).
pub(crate) fn check_sizes(msg: &ClientMsg) -> Result<(), String> {
    let id = |s: &str| if id_ok(s) { Ok(()) } else { Err(format!("{:?} isn't a routine id", s.chars().take(80).collect::<String>())) };
    match msg {
        ClientMsg::SaveRoutine { routine } => routine.clone().validated().map(|_| ()),
        ClientMsg::DeleteRoutine { id: i }
        | ClientMsg::RunRoutine { id: i }
        | ClientMsg::SetRoutineEnabled { id: i, .. }
        | ClientMsg::ListRoutineRuns { id: i } => id(i),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerMsg;

    fn loom() -> Routine {
        Routine {
            id: "loom".into(),
            name: "Loom deals".into(),
            instructions: "Find deals on a new floor loom.".into(),
            when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: "08:00".into(), zone: Some("Europe/Berlin".into()), start: None }],
            ..Routine::default()
        }
    }

    #[test]
    fn routine_messages_round_trip_and_are_checked() {
        let msgs = vec![
            ClientMsg::SaveRoutine { routine: loom() },
            ClientMsg::SaveRoutine { routine: Routine { id: String::new(), ..loom() } },
            ClientMsg::DeleteRoutine { id: "loom".into() },
            ClientMsg::RunRoutine { id: "loom".into() },
            ClientMsg::SetRoutineEnabled { id: "loom".into(), enabled: false },
            ClientMsg::ListRoutineRuns { id: "loom".into() },
        ];
        for m in msgs {
            assert!(m.check_sizes().is_ok(), "{}", m.to_json());
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert!(ClientMsg::RunRoutine { id: "../x".into() }.check_sizes().is_err());
        assert!(ClientMsg::SaveRoutine { routine: Routine { instructions: "x".repeat(9000), ..loom() } }.check_sizes().is_err());
        let server = vec![
            ServerMsg::Routines {
                items: vec![RoutineView {
                    routine: loom(),
                    schedule: vec!["Every day at 8:00 AM".into()],
                    next_run: Some(5),
                    runs_per_month: Some(30),
                    last_run: None,
                }],
            },
            ServerMsg::RoutineRuns {
                routine: "loom".into(),
                runs: vec![RunRecord { chat: Some("c".into()), status: RunStatus::Reported, ..RunRecord::default() }],
            },
            ServerMsg::RoutineEvent {
                routine: "loom".into(),
                name: "Loom deals".into(),
                chat: Some("c".into()),
                kind: RoutineEventKind::Reported,
                title: "Schacht Wolf Pup, 20% off".into(),
                body: "At The Woolery until Sunday.".into(),
            },
        ];
        for m in server {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(ServerMsg::from_json(&json).unwrap(), m, "{json}");
        }
        // a newer engine's event kind still reads
        let e = ServerMsg::from_json(r#"{"type":"routine_event","routine":"r","name":"R","kind":"exploded","title":"t"}"#).unwrap();
        assert!(matches!(e, ServerMsg::RoutineEvent { kind: RoutineEventKind::Other, .. }));
    }
}
