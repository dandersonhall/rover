use std::{
    collections::HashMap,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use dialoguer::Select;
use reqwest::{blocking::Client, Url};
use saucer::{anyhow, Context};
use sysinfo::{Pid, PidExt, ProcessExt, System, SystemExt};

use crate::{
    command::dev::{
        do_dev::handle_rover_error,
        netstat::{get_all_local_endpoints, get_all_local_graphql_endpoints_except},
        socket::{MessageSender, SubgraphName},
    },
    error::RoverError,
    Result,
};

#[derive(Debug)]
pub struct CommandRunner {
    message_sender: MessageSender,
    tasks: HashMap<SubgraphName, BackgroundTask>,
    system: System,
}

impl CommandRunner {
    pub fn new(socket_addr: &str) -> Self {
        Self {
            message_sender: MessageSender::new(socket_addr),
            tasks: HashMap::new(),
            system: System::new(),
        }
    }

    pub fn spawn(&mut self, subgraph_name: SubgraphName, command: String) -> Result<()> {
        for existing_name in self.tasks.keys() {
            if &subgraph_name == existing_name {
                return Err(RoverError::new(anyhow!(
                    "subgraph with name '{}' already has a running process",
                    &subgraph_name
                )));
            }
        }
        let args: Vec<&str> = command.split(' ').collect();
        let (bin, args) = match args.len() {
            0 => Err(anyhow!("the command you passed is empty")),
            1 => Ok((args[0], Vec::new())),
            _ => Ok((args[0], Vec::from_iter(args[1..].iter()))),
        }?;
        tracing::info!("starting `{}`", &command);
        if which::which(bin).is_ok() {
            let mut command = Command::new(bin);
            command.args(args);
            self.tasks
                .insert(subgraph_name, BackgroundTask::new(command)?);
            Ok(())
        } else {
            Err(anyhow!("{} is not installed on this machine", &bin).into())
        }
    }

    pub fn spawn_and_find_url(
        &mut self,
        subgraph_name: SubgraphName,
        command: String,
        client: Client,
        existing_subgraphs: &[Url],
    ) -> Result<Url> {
        let mut preexisting_endpoints = get_all_local_endpoints();
        preexisting_endpoints.extend(existing_subgraphs.iter().cloned());
        self.spawn(subgraph_name, command)?;
        let mut new_graphql_endpoint = None;
        let now = Instant::now();
        while new_graphql_endpoint.is_none() && now.elapsed() < Duration::from_secs(5) {
            let graphql_endpoints =
                get_all_local_graphql_endpoints_except(client.clone(), &preexisting_endpoints);
            match graphql_endpoints.len() {
                0 => {}
                1 => new_graphql_endpoint = Some(graphql_endpoints[0].clone()),
                _ => {
                    if let Ok(endpoint_index) = Select::new()
                        .items(&graphql_endpoints)
                        .default(0)
                        .interact()
                    {
                        new_graphql_endpoint = Some(graphql_endpoints[endpoint_index].clone());
                    }
                }
            }
        }
        if let Some(graphql_endpoint) = new_graphql_endpoint {
            Ok(graphql_endpoint)
        } else {
            Err(RoverError::new(anyhow!(
                "could not find GraphQL endpoint after 5 seconds"
            )))
        }
    }

    pub fn kill_tasks(&mut self) {
        if !self.tasks.is_empty() {
            let num_tasks = self.tasks.len();
            tracing::info!("dropping {} spawned background tasks", num_tasks);
            self.system.refresh_all();
            for (subgraph_name, background_task) in &self.tasks {
                let _ = self
                    .message_sender
                    .remove_subgraph(subgraph_name)
                    .map_err(handle_rover_error);
                if let Some(process) = self.system.process(background_task.pid()) {
                    if !process.kill() {
                        eprintln!(
                            "warn: could not drop process with PID {}",
                            background_task.pid()
                        );
                    }
                }
            }
        }
        tracing::info!("done dropping tasks");
    }
}

impl Drop for CommandRunner {
    fn drop(&mut self) {
        self.kill_tasks()
    }
}

#[derive(Debug)]
struct BackgroundTask {
    child: Child,
}

impl BackgroundTask {
    fn new(mut command: Command) -> Result<Self> {
        if cfg!(windows) {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let child = command
            .spawn()
            .with_context(|| "could not spawn child process")?;
        Ok(Self { child })
    }

    fn pid(&self) -> Pid {
        Pid::from_u32(self.child.id())
    }
}