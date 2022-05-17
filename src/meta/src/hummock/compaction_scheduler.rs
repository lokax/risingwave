// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use risingwave_hummock_sdk::compact::compact_task_to_string;
use risingwave_hummock_sdk::compaction_group::CompactionGroupId;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::hummock::error::Error;
use crate::hummock::{CompactorManagerRef, HummockManagerRef};
use crate::storage::MetaStore;

pub type CompactionSchedulerRef<S> = Arc<CompactionScheduler<S>>;

pub type CompactionRequestChannelRef = Arc<CompactionRequestChannel>;
/// [`CompactionRequestChannel`] wrappers a mpsc channel and deduplicate requests from same
/// compaction groups.
pub struct CompactionRequestChannel {
    request_tx: UnboundedSender<CompactionGroupId>,
    request_rx: Mutex<Option<UnboundedReceiver<CompactionGroupId>>>,
    scheduled: Mutex<HashSet<CompactionGroupId>>,
}

impl CompactionRequestChannel {
    fn new(
        request_tx: UnboundedSender<CompactionGroupId>,
        request_rx: UnboundedReceiver<CompactionGroupId>,
    ) -> Self {
        Self {
            request_tx,
            request_rx: Mutex::new(Some(request_rx)),
            scheduled: Default::default(),
        }
    }

    /// Enqueues only if the target is not yet in queue.
    pub fn try_send(&self, compaction_group: CompactionGroupId) -> bool {
        let mut guard = self.scheduled.lock();
        if guard.get(&compaction_group).is_some() {
            return false;
        }
        if self.request_tx.send(compaction_group).is_ok() {
            guard.insert(compaction_group);
            return true;
        }
        false
    }

    fn unschedule(&self, compaction_group: CompactionGroupId) {
        self.scheduled.lock().remove(&compaction_group);
    }
}

/// Schedules compaction task picking and assignment.
pub struct CompactionScheduler<S>
where
    S: MetaStore,
{
    hummock_manager: HummockManagerRef<S>,
    compactor_manager: CompactorManagerRef,
    shutdown_tx: UnboundedSender<()>,
    shutdown_rx: Mutex<Option<UnboundedReceiver<()>>>,
    request_channel: CompactionRequestChannelRef,
}

impl<S> CompactionScheduler<S>
where
    S: MetaStore,
{
    pub fn new(
        hummock_manager: HummockManagerRef<S>,
        compactor_manager: CompactorManagerRef,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
        let request_channel = Arc::new(CompactionRequestChannel::new(request_tx, request_rx));
        Self {
            hummock_manager,
            compactor_manager,
            shutdown_tx,
            shutdown_rx: Mutex::new(Some(shutdown_rx)),
            request_channel,
        }
    }

    pub async fn start(&self) {
        let (mut shutdown_rx, mut request_rx) = match (
            self.shutdown_rx.lock().take(),
            self.request_channel.request_rx.lock().take(),
        ) {
            (Some(shutdown_rx), Some(request_rx)) => (shutdown_rx, request_rx),
            _ => {
                tracing::warn!("Compaction scheduler is already started");
                return;
            }
        };
        self.hummock_manager
            .set_compaction_scheduler(self.request_channel.clone());
        tracing::info!("Start compaction scheduler.");
        'compaction_trigger: loop {
            let compaction_group: CompactionGroupId = tokio::select! {
                compaction_group = request_rx.recv() => {
                    match compaction_group {
                        Some(compaction_group) => compaction_group,
                        None => {
                            break 'compaction_trigger;
                        }
                    }
                },
                // Shutdown compactor
                _ = shutdown_rx.recv() => {
                    break 'compaction_trigger;
                }
            };
            self.request_channel.unschedule(compaction_group);
            self.pick_and_assign(compaction_group).await;
        }
        tracing::info!("Compaction scheduler is stopped");
    }

    async fn pick_and_assign(&self, compaction_group: CompactionGroupId) {
        // 1. Pick a compaction task.
        // TODO: specify compaction_group in get_compact_task
        let compact_task = match self.hummock_manager.get_compact_task().await {
            Ok(Some(compact_task)) => compact_task,
            Ok(None) => {
                // No compaction task available.
                return;
            }
            Err(err) => {
                tracing::warn!("Failed to get compaction task: {:#?}.", err);
                return;
            }
        };
        tracing::trace!(
            "Picked compaction task. {}",
            compact_task_to_string(&compact_task)
        );

        // 2. Assign the compaction task to a compactor.
        'send_task: loop {
            // 2.1 Select a compactor.
            let compactor = match self.compactor_manager.next_compactor() {
                None => {
                    tracing::warn!("No compactor is available.");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue 'send_task;
                }
                Some(compactor) => compactor,
            };
            // TODO: skip busy compactor

            // 2.2 Send the compaction task to the compactor.
            let send_task = async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    compactor
                        .send_task(Some(compact_task.clone()), None)
                        .await
                        .is_ok()
                })
                .await
                .unwrap_or(false)
            };
            match self
                .hummock_manager
                .assign_compaction_task(&compact_task, compactor.context_id(), send_task)
                .await
            {
                Ok(_) => {
                    // Reschedule it in case there are more tasks from this compaction group.
                    self.request_channel.try_send(compaction_group);
                    // TODO: timeout assigned compaction task
                    tracing::trace!(
                        "Assigned compaction task. {}",
                        compact_task_to_string(&compact_task)
                    );
                    break 'send_task;
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to assign compaction task to compactor {}: {:#?}",
                        compactor.context_id(),
                        err
                    );
                    match err {
                        Error::InvalidContext(_) | Error::CompactorUnreachable(_) => {
                            self.compactor_manager
                                .remove_compactor(compactor.context_id());
                        }
                        _ => {}
                    }
                    continue 'send_task;
                }
            }
        }
    }

    pub fn shutdown_sender(&self) -> UnboundedSender<()> {
        self.shutdown_tx.clone()
    }
}
