use anyhow::Result;
use log::{debug, error};

use pueue_lib::process_helper::*;
use pueue_lib::settings::Settings;

use super::state_helper::LockedState;

pub mod kill;
pub mod pause;
pub mod spawn;
pub mod start;

pub use pueue_lib::network::message::{Shutdown, TaskSelection};
pub use pueue_lib::process_helper::ProcessAction;

/// Initiate shutdown, which includes killing all children and pausing all groups.
/// We don't have to pause any groups, as no new tasks will be spawned during shutdown anyway.
/// Any groups with queued tasks, will be automatically paused on state-restoration.
pub fn initiate_shutdown(settings: &Settings, state: &mut LockedState, shutdown: Shutdown) {
    state.shutdown = Some(shutdown);

    kill::kill(settings, state, TaskSelection::All, false, None);
}

/// This is a small wrapper around the real platform dependant process handling logic
/// It only ensures, that the process we want to manipulate really does exists.
pub fn perform_action(state: &mut LockedState, id: usize, action: ProcessAction) -> Result<bool> {
    match state.children.get_child_mut(id) {
        Some(child) => {
            debug!("Executing action {action:?} to {id}");
            send_signal_to_child(child, &action)?;

            Ok(true)
        }
        None => {
            error!("Tried to execute action {action:?} to non existing task {id}");
            Ok(false)
        }
    }
}
