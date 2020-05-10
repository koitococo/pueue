use ::std::collections::{BTreeMap, HashMap};
use ::std::io::Write;
use ::std::process::Stdio;
use ::std::process::{Child, Command};
use ::std::sync::mpsc::Receiver;
use ::std::time::Duration;

use ::anyhow::Result;
use ::chrono::prelude::*;
use ::handlebars::Handlebars;
use ::log::{debug, error, info, warn};
#[cfg(not(windows))]
use ::nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};

use ::pueue::log::*;
use ::pueue::message::*;
use ::pueue::state::SharedState;
use ::pueue::task::{Task, TaskResult, TaskStatus};

pub struct TaskHandler {
    state: SharedState,
    receiver: Receiver<Message>,
    children: BTreeMap<usize, Child>,
    callbacks: Vec<Child>,
    running: bool,
    reset: bool,
    // Some static settings that are extracted from `state.settings`
    // for convenience purposes.
    pueue_directory: String,
    callback: Option<String>,
    pause_on_failure: bool,
}

impl TaskHandler {
    pub fn new(state: SharedState, receiver: Receiver<Message>) -> Self {
        let (running, pueue_directory, callback, pause_on_failure) = {
            let state = state.lock().unwrap();
            let settings = &state.settings.daemon;
            (
                state.running,
                settings.pueue_directory.clone(),
                settings.callback.clone(),
                settings.pause_on_failure,
            )
        };
        TaskHandler {
            state,
            receiver,
            children: BTreeMap::new(),
            callbacks: Vec::new(),
            running,
            reset: false,
            pueue_directory,
            callback,
            pause_on_failure,
        }
    }
}

/// The task handler needs to kill all child processes as soon, as the program exits
/// This is needed to prevent detached processes
impl Drop for TaskHandler {
    fn drop(&mut self) {
        let ids: Vec<usize> = self.children.keys().cloned().collect();
        for id in ids {
            let mut child = self.children.remove(&id).expect("Failed killing children");
            info!("Killing child {}", id);
            let _ = child.kill();
        }
    }
}

impl TaskHandler {
    /// Main loop of the task handler
    /// In here a few things happen:
    /// 1. Propagated commands from socket communication is received and handled
    /// 2. Check whether any tasks just finished
    /// 3. Check if there are any stashed processes ready for being enqueued
    /// 4. Check whether we can spawn new tasks
    pub fn run(&mut self) {
        loop {
            // Sleep for a few milliseconds. We don't want to hurt the CPU
            let timeout = Duration::from_millis(100);
            // Don't use recv_timeout for now, until this bug get's fixed
            // https://github.com/rust-lang/rust/issues/39364
            //match self.receiver.recv_timeout(timeout) {
            std::thread::sleep(timeout);

            self.receive_commands();
            self.handle_finished_tasks();
            self.check_callbacks();
            self.check_stashed();
            self.check_failed_dependencies();
            if self.running && !self.reset {
                let _res = self.check_new();
            }
        }
    }

    /// Search and return the next task that can be started.
    /// Precondition for a task to be started:
    /// - is in Queued state
    /// - There are free slots in the task's group
    /// - has all its dependencies in `Done` state
    pub fn get_next_task_id(&mut self) -> Option<usize> {
        let state = self.state.lock().unwrap();

        // Check how many tasks are running in each group
        let mut running_tasks_per_group: HashMap<String, usize> = HashMap::new();
        // Create a default group for tasks without an explicit group
        running_tasks_per_group.insert("default".into(), 0);

        for (_, task) in state.tasks.iter() {
            // We are only interested in currently running tasks.
            if ![TaskStatus::Running, TaskStatus::Paused].contains(&task.status) {
                continue;
            }

            // Get the group of the task or the default key
            let group = if let Some(group) = &task.group {
                group
            } else {
                "default"
            };

            match running_tasks_per_group.get(group) {
                Some(&count) => {
                    running_tasks_per_group.insert(group.into(), count + 1);
                }
                None => {
                    running_tasks_per_group.insert(group.into(), 1);
                }
            }
        }

        return state
            .tasks
            .iter()
            .filter(|(_, task)| task.status == TaskStatus::Queued)
            .filter(|(_, task)| {
                if let Some(group) = &task.group {
                    // The task is assigned to a group.
                    //
                    // If there's no entry, we can safely return true, since there's no running
                    // task for this group yet.
                    //
                    // If there's a group, we have to ensure that there are fewer running tasks than
                    // allowed for this group.
                    match running_tasks_per_group.get(group) {
                        Some(count) => match state.settings.daemon.groups.get(group) {
                            Some(allowed) => count < allowed,
                            None => {
                                error!(
                                    "Got task with unknown group {}. Please report this!",
                                    group
                                );
                                false
                            }
                        },
                        None => true,
                    }
                } else {
                    // We can unwrap safely, since default is always created.
                    let running = running_tasks_per_group.get("default").unwrap();

                    running < &state.settings.daemon.default_parallel_tasks
                }
            })
            .filter(|(_, task)| {
                // Check whether all dependencies for this task are fulfilled.
                task.dependencies
                    .iter()
                    .flat_map(|id| state.tasks.get(id))
                    .all(|task| task.status == TaskStatus::Done)
            })
            .next()
            .map(|(id, _)| *id);
    }

    /// See if we can start a new queued task.
    fn check_new(&mut self) -> Result<()> {
        // Get the next task id that can be started
        if let Some(id) = self.get_next_task_id() {
            self.start_process(id);
        }

        Ok(())
    }

    /// Ensure that no `Queued` tasks have any failed dependencies.
    /// Otherwise set their status to `Done` and result to `DependencyFailed`.
    pub fn check_failed_dependencies(&mut self) {
        let mut state = self.state.lock().unwrap();
        let has_failed_deps: Vec<_> = state
            .tasks
            .iter()
            .filter(|(_, task)| task.status == TaskStatus::Queued)
            .filter_map(|(id, task)| {
                let failed = task
                    .dependencies
                    .iter()
                    .flat_map(|id| state.tasks.get(id))
                    .filter(|task| task.failed())
                    .map(|task| task.id)
                    .next();

                failed.map(|f| (*id, f))
            })
            .collect();

        for (id, _) in has_failed_deps {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.status = TaskStatus::Done;
                task.result = Some(TaskResult::DependencyFailed);
            }
        }
    }

    /// Actually spawn a new sub process
    /// The output of subprocesses is piped into a seperate file for easier access
    fn start_process(&mut self, task_id: usize) {
        // Already get the mutex here to ensure that the state won't be manipulated
        // while we are looking for a task to start.
        let mut state = self.state.lock().unwrap();

        let task = state.tasks.get_mut(&task_id);
        let task = match task {
            Some(task) => {
                if !vec![TaskStatus::Stashed, TaskStatus::Queued, TaskStatus::Paused]
                    .contains(&task.status)
                {
                    info!("Tried to start task with status: {}", task.status);
                    return;
                }
                task
            }
            None => {
                info!("Tried to start non-existing task: {}", task_id);
                return;
            }
        };
        // In case a task that has been scheduled for enqueueing, is forcefully
        // started by hand, set `enqueue_at` to `None`.
        task.enqueue_at = None;

        // Try to get the log files to which the output of the process
        // Will be written. Error if this doesn't work!
        let (stdout_log, stderr_log) = match create_log_file_handles(task_id, &self.pueue_directory)
        {
            Ok((out, err)) => (out, err),
            Err(err) => {
                error!("Failed to create child log files: {:?}", err);
                return;
            }
        };

        // Spawn the actual subprocess
        let mut spawn_command = Command::new(if cfg!(windows) { "powershell" } else { "sh" });

        if cfg!(windows) {
            // Chain two `powershell` commands, one that sets the output encoding to utf8 and then the user provided one.
            spawn_command.arg("-c").arg(format!(
                "[Console]::OutputEncoding = [Text.UTF8Encoding]::UTF8; {}",
                task.command
            ));
        } else {
            spawn_command.arg("-c").arg(&task.command);
        }

        let spawn_result = spawn_command
            .current_dir(&task.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            .spawn();

        // Check if the task managed to spawn
        let child = match spawn_result {
            Ok(child) => child,
            Err(err) => {
                let error = format!("Failed to spawn child {} with err: {:?}", task_id, err);
                error!("{}", error);
                clean_log_handles(task_id, &self.pueue_directory);
                task.status = TaskStatus::Done;
                task.result = Some(TaskResult::FailedToSpawn(error));

                // Pause the daemon, if the settings say so
                if self.pause_on_failure {
                    self.running = false;
                    state.running = false;
                    state.save();
                }
                return;
            }
        };
        self.children.insert(task_id, child);
        info!("Started task: {}", task.command);

        task.start = Some(Local::now());
        task.status = TaskStatus::Running;

        state.save();
    }

    /// As time passes, some delayed tasks may need to be enqueued.
    /// Gather all stashed tasks and enqueue them if it is after the task's enqueue_at
    fn check_stashed(&mut self) {
        let mut state = self.state.lock().unwrap();

        let mut changed = false;
        for (_, task) in state.tasks.iter_mut() {
            if task.status != TaskStatus::Stashed {
                continue;
            }

            if let Some(time) = task.enqueue_at {
                if time <= Local::now() {
                    info!("Enqueuing delayed task : {}", task.id);

                    task.status = TaskStatus::Queued;
                    task.enqueue_at = None;
                    changed = true;
                }
            }
        }
        // Save the state if a task has been enqueued
        if changed {
            state.save();
        }
    }

    /// Check whether there are any finished processes
    /// In case there are, handle them and update the shared state
    fn handle_finished_tasks(&mut self) {
        let (finished, errored) = self.get_finished();

        // The daemon got a reset request and all children already finished
        if self.reset && self.children.is_empty() {
            let mut state = self.state.lock().unwrap();
            state.reset();
            self.running = true;
            self.reset = false;
        }

        // Nothing to do. Early return
        if finished.is_empty() && errored.is_empty() {
            return;
        }

        let state_ref = self.state.clone();
        let mut state = state_ref.lock().unwrap();
        // We need to know if there are any failed tasks,
        // in case the user wants to stop the daemon if a tasks fails
        let mut failed_task_exists = false;

        for task_id in finished.iter() {
            let mut child = self.children.remove(task_id).expect("Child went missing");

            let exit_code = child.wait().unwrap().code();
            let mut task = state.tasks.get_mut(&task_id).unwrap();
            // Only processes with exit code 0 exited successfully
            if exit_code == Some(0) {
                task.result = Some(TaskResult::Success);
            // Tasks with an exit code != 0 did fail in some kind of way
            } else if let Some(exit_code) = exit_code {
                task.result = Some(TaskResult::Failed(exit_code));
                failed_task_exists = true;
            }

            task.status = TaskStatus::Done;
            task.end = Some(Local::now());

            // Already remove the output files, if the daemon is being reset anyway
            if self.reset {
                clean_log_handles(*task_id, &self.pueue_directory);
            }
            self.spawn_callback(&task);
        }

        // Handle errored tasks
        // TODO: This could be be refactored. Let's try to combine finished and error handling.
        for task_id in errored.iter() {
            let _child = self.children.remove(task_id).expect("Child went missing");
            let mut task = state.tasks.get_mut(&task_id).unwrap();
            task.status = TaskStatus::Done;
            task.result = Some(TaskResult::Killed);
            failed_task_exists = true;
            self.spawn_callback(&task);
        }

        // Pause the daemon, if the settings say so and some process failed
        if failed_task_exists && self.pause_on_failure {
            self.running = false;
            state.running = false;
        }

        state.save()
    }

    /// Gather all finished tasks and sort them by finished and errored.
    /// Returns two lists of task ids, namely finished_task_ids and errored _task_ids
    fn get_finished(&mut self) -> (Vec<usize>, Vec<usize>) {
        let mut finished = Vec::new();
        let mut errored = Vec::new();
        for (id, child) in self.children.iter_mut() {
            match child.try_wait() {
                // Handle a child error.
                Err(error) => {
                    info!("Task {} failed with error {:?}", id, error);
                    errored.push(*id);
                }
                // Child process did not exit yet
                Ok(None) => continue,
                Ok(_exit_status) => {
                    info!("Task {} just finished", id);
                    finished.push(*id);
                }
            }
        }
        (finished, errored)
    }

    /// Some client instructions require immediate action by the task handler
    /// These commands are
    fn receive_commands(&mut self) {
        match self.receiver.try_recv() {
            Ok(message) => self.handle_message(message),
            Err(_) => {}
        };
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::Pause(message) => self.pause(message),
            Message::Start(message) => self.start(message),
            Message::Kill(message) => self.kill(message),
            Message::Send(message) => self.send(message),
            Message::Reset => self.reset(),
            _ => info!("Received unhandled message {:?}", message),
        }
    }

    /// Send a signal to a unix process
    #[cfg(not(windows))]
    fn send_signal(&mut self, id: usize, signal: Signal) -> Result<bool, nix::Error> {
        if let Some(child) = self.children.get(&id) {
            debug!("Sending signal {} to {}", signal, id);
            let pid = Pid::from_raw(child.id() as i32);
            signal::kill(pid, signal)?;
            return Ok(true);
        };

        error!(
            "Tried to send signal {} to non existing child {}",
            signal, id
        );
        Ok(false)
    }

    /// Handle the start message:
    /// 1. Either start the daemon and all tasks.
    /// 2. Or force the start of specific tasks.
    fn start(&mut self, task_ids: Vec<usize>) {
        // Only start specific tasks
        if !task_ids.is_empty() {
            for id in &task_ids {
                // Continue all children that are simply paused
                if self.children.contains_key(id) {
                    self.continue_task(*id);
                } else {
                    // Start processes for all tasks that haven't been started yet
                    self.start_process(*id);
                }
            }
            return;
        }

        // Start the daemon and all paused tasks
        let keys: Vec<usize> = self.children.keys().cloned().collect();
        for id in keys {
            self.continue_task(id);
        }
        info!("Resuming daemon (start)");
        self.change_running(true);
    }

    /// Send a start signal to a paused task to continue execution
    fn continue_task(&mut self, id: usize) {
        if !self.children.contains_key(&id) {
            return;
        }
        {
            // Task is already done
            let state = self.state.lock().unwrap();
            if state.tasks.get(&id).unwrap().is_done() {
                return;
            }
        }
        #[cfg(windows)]
        {
            warn!("Failed starting task {}: not supported on windows.", id);
        }
        #[cfg(not(windows))]
        {
            match self.send_signal(id, Signal::SIGCONT) {
                Err(err) => warn!("Failed starting task {}: {:?}", id, err),
                Ok(success) => {
                    if success {
                        let mut state = self.state.lock().unwrap();
                        state.change_status(id, TaskStatus::Running);
                    }
                }
            }
        }
    }

    /// Handle the pause message:
    /// 1. Either pause the daemon and all tasks.
    /// 2. Or only pause specific tasks.
    fn pause(&mut self, message: PauseMessage) {
        // Only pause specific tasks
        if !message.task_ids.is_empty() {
            for id in &message.task_ids {
                self.pause_task(*id);
            }
            return;
        }

        // Pause the daemon and all tasks
        let keys: Vec<usize> = self.children.keys().cloned().collect();
        if !message.wait {
            for id in keys {
                self.pause_task(id);
            }
        }
        info!("Pausing daemon");
        self.change_running(false);
    }

    /// Pause a specific task.
    /// Send a signal to the process to actually pause the OS process
    fn pause_task(&mut self, id: usize) {
        if !self.children.contains_key(&id) {
            return;
        }
        #[cfg(windows)]
        {
            info!("Failed pausing task {}: not supported on windows.", id);
        }
        #[cfg(not(windows))]
        {
            match self.send_signal(id, Signal::SIGSTOP) {
                Err(err) => info!("Failed pausing task {}: {:?}", id, err),
                Ok(success) => {
                    if success {
                        let mut state = self.state.lock().unwrap();
                        state.change_status(id, TaskStatus::Paused);
                    }
                }
            }
        }
    }

    /// Handle the pause message:
    /// 1. Either kill all tasks.
    /// 2. Or only kill specific tasks.
    fn kill(&mut self, message: KillMessage) {
        // Only pause specific tasks
        if !message.task_ids.is_empty() {
            for id in message.task_ids {
                self.kill_task(id);
            }
            return;
        }

        // Pause the daemon and kill all tasks
        if message.all {
            info!("Killing all spawned children");
            self.change_running(false);
            let keys: Vec<usize> = self.children.keys().cloned().collect();
            for id in keys {
                self.kill_task(id);
            }
        }
    }

    /// Kill a specific task and handle it accordingly
    /// Triggered on `reset` and `kill`.
    fn kill_task(&mut self, task_id: usize) {
        if let Some(child) = self.children.get_mut(&task_id) {
            match child.kill() {
                Err(_) => debug!("Task {} has already finished by itself", task_id),
                _ => {
                    // Already mark the task as killed over here.
                    // It's hard to distinguish whether it's killed later on.
                    let mut state = self.state.lock().unwrap();
                    let mut task = state.tasks.get_mut(&task_id).unwrap();
                    task.status = TaskStatus::Done;
                    task.result = Some(TaskResult::Killed);
                }
            };
        } else {
            warn!("Tried to kill non-existing child: {}", task_id);
        }
    }

    /// Send some input to a child process
    fn send(&mut self, message: SendMessage) {
        let task_id = message.task_id;
        let input = message.input;
        let child = match self.children.get_mut(&task_id) {
            Some(child) => child,
            None => {
                warn!(
                    "Task {} finished before input could be sent: {}",
                    task_id, input
                );
                return;
            }
        };
        {
            let child_stdin = child.stdin.as_mut().unwrap();
            if let Err(err) = child_stdin.write_all(&input.clone().into_bytes()) {
                warn!(
                    "Failed to send input to task {} with err {:?}: {}",
                    task_id, err, input
                );
            };
        }
    }

    /// Kill all children by reusing the `kill` function
    /// Set the `reset` flag, which will prevent new tasks from being spawned.
    /// If all children finished, the state will be completely reset.
    fn reset(&mut self) {
        let message = KillMessage {
            task_ids: Vec::new(),
            all: true,
        };
        self.kill(message);

        self.reset = true;
    }

    /// Change the running state consistently
    fn change_running(&mut self, running: bool) {
        let mut state = self.state.lock().unwrap();
        state.running = running;
        self.running = running;
        state.save();
    }

    /// Users can specify a callback that's fired whenever a task finishes
    /// Execute the callback by spawning a new subprocess.
    fn spawn_callback(&mut self, task: &Task) {
        // Return early, if there's no callback specified
        let callback = if let Some(callback) = &self.callback {
            callback
        } else {
            return;
        };

        // Build the callback command from the given template
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        // Build templating variables
        let mut parameters = HashMap::new();
        parameters.insert("id", task.id.to_string());
        parameters.insert("command", task.command.clone());
        parameters.insert("path", task.path.clone());
        parameters.insert("result", task.result.clone().unwrap().to_string());
        let callback_command = match handlebars.render_template(&callback, &parameters) {
            Ok(command) => command,
            Err(err) => {
                error!(
                    "Failed to create callback command from template with error: {}",
                    err
                );
                return;
            }
        };

        let mut spawn_command = Command::new(if cfg!(windows) { "powershell" } else { "sh" });
        if cfg!(windows) {
            // Chain two `powershell` commands, one that sets the output encoding to utf8 and then the user provided one.
            spawn_command.arg("-c").arg(format!(
                "[Console]::OutputEncoding = [Text.UTF8Encoding]::UTF8; {}",
                callback_command
            ));
        } else {
            spawn_command.arg("-c").arg(&callback_command);
        }

        // Spawn the callback subprocess and log if it fails.
        let spawn_result = spawn_command.spawn();
        let child = match spawn_result {
            Err(error) => {
                error!("Failed to spawn callback with error: {}", error);
                return;
            }
            Ok(child) => child,
        };

        debug!("Spawned callback for task {}", task.id);
        self.callbacks.push(child);
    }

    /// Look at all running callbacks and log any errors
    /// If everything went smoothly, simply remove them from the list
    fn check_callbacks(&mut self) {
        let mut finished = Vec::new();
        for (id, child) in self.callbacks.iter_mut().enumerate() {
            match child.try_wait() {
                // Handle a child error.
                Err(error) => {
                    error!("Callback failed with error {:?}", error);
                    finished.push(id);
                }
                // Child process did not exit yet
                Ok(None) => continue,
                Ok(exit_status) => {
                    info!("Callback finished with exit code {:?}", exit_status);
                    finished.push(id);
                }
            }
        }

        finished.reverse();
        for id in finished.iter() {
            self.callbacks.remove(*id);
        }
    }
}
