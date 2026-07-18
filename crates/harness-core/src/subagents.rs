use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::actors::{
    ActorHandle, ActorReceiver, ActorSender, DEFAULT_ACTOR_MAILBOX_CAPACITY,
    channel as actor_channel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Stable identifier for a subagent tracked by the root harness.
pub struct AgentId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Current lifecycle status for a subagent.
pub enum AgentStatus {
    /// Agent is actively running.
    Running,
    /// Agent is waiting for input or work.
    Waiting,
    /// Agent completed with a final message.
    Completed(String),
    /// Agent failed with an error message.
    Failed(String),
    /// Agent was interrupted by user or scheduler action.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Renderable summary for one subagent.
pub struct AgentSummary {
    /// Stable agent identifier.
    pub id: AgentId,
    /// Agent working path or display path.
    pub path: String,
    /// Current agent status.
    pub status: AgentStatus,
    /// Last task-level message reported by the agent.
    pub last_task_message: Option<String>,
    /// Last activity message shown in the UI.
    pub last_activity_message: Option<String>,
}

#[derive(Debug, Default)]
/// Registry of known subagents keyed by id.
pub struct AgentRegistry {
    agents: HashMap<AgentId, AgentSummary>,
}

impl AgentRegistry {
    /// Insert or replace an agent summary.
    pub fn upsert(&mut self, summary: AgentSummary) {
        self.agents.insert(summary.id, summary);
    }

    /// Remove an agent summary, returning the removed summary if present.
    pub fn remove(&mut self, id: AgentId) -> Option<AgentSummary> {
        self.agents.remove(&id)
    }

    /// Return agent summaries sorted by display path.
    pub fn list(&self) -> Vec<AgentSummary> {
        let mut agents = self.agents.values().cloned().collect::<Vec<_>>();
        agents.sort_by(|left, right| left.path.cmp(&right.path));
        agents
    }
}

#[derive(Debug)]
/// Commands accepted by the subagent scheduler.
pub enum AgentSchedulerCommand {
    /// Insert or replace an agent summary.
    /// Insert or replace an agent summary.
    Upsert(AgentSummary),
    /// Remove an agent from the registry.
    Remove(AgentId),
    /// Queue a message for one agent.
    QueueMessage {
        /// Agent receiving the message.
        agent_id: AgentId,
        /// Message to queue.
        message: String,
    },
    /// Interrupt one agent with a message.
    Interrupt {
        /// Agent receiving the interrupt.
        agent_id: AgentId,
        /// Interrupt message.
        message: String,
    },
    /// Shut down the scheduler.
    Shutdown,
}

#[derive(Debug, Clone)]
/// Events emitted by the subagent scheduler.
pub enum AgentSchedulerEvent {
    /// An agent summary changed.
    /// An agent summary changed.
    AgentUpdated(AgentSummary),
    /// An agent was removed from the registry.
    AgentRemoved(AgentId),
    /// A mailbox message was queued.
    MailboxQueued {
        /// Agent whose mailbox changed.
        agent_id: AgentId,
    },
    /// An interrupt was delivered.
    Interrupted {
        /// Agent receiving the interrupt.
        agent_id: AgentId,
    },
    /// Scheduler shutdown completed.
    ShutdownComplete,
}

/// Actor that tracks subagent summaries and mailbox notifications.
pub struct AgentScheduler {
    registry: AgentRegistry,
    rx: ActorReceiver<AgentSchedulerCommand>,
    events: ActorSender<AgentSchedulerEvent>,
}

impl AgentScheduler {
    /// Spawn a subagent scheduler actor.
    pub fn spawn(events: ActorSender<AgentSchedulerEvent>) -> ActorHandle<AgentSchedulerCommand> {
        let (handle, rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let scheduler = Self {
            registry: AgentRegistry::default(),
            rx,
            events,
        };
        tokio::spawn(scheduler.run());
        handle
    }

    async fn run(mut self) {
        while let Ok(command) = self.rx.recv().await {
            match command {
                AgentSchedulerCommand::Upsert(summary) => {
                    self.registry.upsert(summary.clone());
                    let _ = self
                        .events
                        .send(AgentSchedulerEvent::AgentUpdated(summary))
                        .await;
                }
                AgentSchedulerCommand::Remove(agent_id) => {
                    if self.registry.remove(agent_id).is_some() {
                        let _ = self
                            .events
                            .send(AgentSchedulerEvent::AgentRemoved(agent_id))
                            .await;
                    }
                }
                AgentSchedulerCommand::QueueMessage {
                    agent_id,
                    message: _,
                } => {
                    let _ = self
                        .events
                        .send(AgentSchedulerEvent::MailboxQueued { agent_id })
                        .await;
                }
                AgentSchedulerCommand::Interrupt {
                    agent_id,
                    message: _,
                } => {
                    let _ = self
                        .events
                        .send(AgentSchedulerEvent::Interrupted { agent_id })
                        .await;
                }
                AgentSchedulerCommand::Shutdown => {
                    let _ = self
                        .events
                        .send(AgentSchedulerEvent::ShutdownComplete)
                        .await;
                    break;
                }
            }
        }
    }
}
