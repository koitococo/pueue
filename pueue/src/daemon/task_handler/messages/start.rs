use log::{error, info, warn};

use pueue_lib::network::message::TaskSelection;
use pueue_lib::state::GroupStatus;
use pueue_lib::task::TaskStatus;

use crate::daemon::state_helper::{save_state, LockedState};
use crate::daemon::task_handler::{ProcessAction, Shutdown, TaskHandler};
use crate::ok_or_shutdown;

impl TaskHandler {
    /// Start specific tasks or groups.
    ///
    /// By default, this command only resumes tasks.
    /// However, if specific task_ids are provided, tasks can actually be force-started.
    /// Of course, they can only be started if they're in a valid status, i.e. Queued/Stashed.
    pub fn start(&mut self, tasks: TaskSelection) {
        let cloned_state_mutex = self.state.clone();
        let mut state = cloned_state_mutex.lock().unwrap();

        let task_ids = match tasks {
            TaskSelection::TaskIds(task_ids) => {
                // Start specific tasks.
                // This is handled differently and results in an early return, as this branch is
                // capable of force-spawning processes, instead of simply resuming tasks.
                for task_id in task_ids {
                    // Continue all children that are simply paused
                    if self.children.has_child(task_id) {
                        self.continue_task(&mut state, task_id);
                    } else {
                        // Start processes for all tasks that haven't been started yet
                        self.start_process(task_id, &mut state);
                    }
                }
                ok_or_shutdown!(self, save_state(&state, &self.settings));
                return;
            }
            TaskSelection::Group(group_name) => {
                // Ensure that a given group exists. (Might not happen due to concurrency)
                let group = match state.groups.get_mut(&group_name) {
                    Some(group) => group,
                    None => return,
                };

                // Set the group to running.
                group.status = GroupStatus::Running;
                info!("Resuming group {}", &group_name);

                let filtered_tasks = state.filter_tasks_of_group(
                    |task| matches!(task.status, TaskStatus::Paused),
                    &group_name,
                );

                filtered_tasks.matching_ids
            }
            TaskSelection::All => {
                // Resume all groups and the default queue
                info!("Resuming everything");
                state.set_status_for_all_groups(GroupStatus::Running);

                self.children.all_task_ids()
            }
        };

        // Resume all specified paused tasks
        for task_id in task_ids {
            self.continue_task(&mut state, task_id);
        }

        ok_or_shutdown!(self, save_state(&state, &self.settings));
    }

    /// Send a start signal to a paused task to continue execution.
    fn continue_task(&mut self, state: &mut LockedState, task_id: usize) {
        // Task doesn't exist
        if !self.children.has_child(task_id) {
            return;
        }

        // Task is already done
        if state.tasks.get(&task_id).unwrap().is_done() {
            return;
        }

        let success = match self.perform_action(task_id, ProcessAction::Resume) {
            Err(err) => {
                warn!("Failed to resume task {}: {:?}", task_id, err);
                false
            }
            Ok(success) => success,
        };

        if success {
            state.change_status(task_id, TaskStatus::Running);
        }
    }
}
