//! When things happen: wall-clock times in zones (`time`), recurrence rules (`recur`, RFC
//! 5545's RRULE subset) and schedules (`schedule`: the next time a routine is due, and
//! "Every day at 8:00 AM" for people). No I/O beyond reading the system's zone. Used by the
//! calendar backend and the engine's routines.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod recur;
pub mod schedule;
pub mod time;

pub use schedule::Schedule;
