//! Retained assignments and results, independent of PTY lifetime.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime};

use crate::agent::AgentId;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 512;
const MAX_RESULT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Assigned,
    Complete,
    Accepted,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub summary: String,
    pub work_dir: PathBuf,
    pub artifacts: Vec<String>,
    pub validation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub assignment: String,
    pub state: TaskState,
    pub result: Option<TaskResult>,
    pub archived: bool,
    pub updated_at: SystemTime,
}

/// Compact inbox entries keep a full history listing below the IPC response limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub state: TaskState,
    pub summary: String,
    pub archived: bool,
}

impl From<TaskRecord> for TaskSummary {
    fn from(task: TaskRecord) -> Self {
        let text = task.result.as_ref().map(|r| r.summary.as_str()).unwrap_or(&task.assignment);
        Self { agent_id: task.agent_id, parent_id: task.parent_id, state: task.state,
            summary: text.chars().take(120).collect(), archived: task.archived }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Journal {
    records: BTreeMap<String, TaskRecord>,
}

#[derive(Default)]
pub struct TaskStore {
    records: Mutex<Journal>,
    changed: Condvar,
    path: Option<PathBuf>,
}

impl TaskStore {
    pub fn open(path: &Path) -> Result<Self> {
        let mut records: Journal = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid task journal; existing file was preserved")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(e) => return Err(e.into()),
        };
        for task in records.records.values_mut() {
            if task.state == TaskState::Assigned {
                task.state = TaskState::Interrupted;
                task.updated_at = SystemTime::now();
            }
        }
        let store = Self {
            records: Mutex::new(records),
            changed: Condvar::new(),
            path: Some(path.to_owned()),
        };
        store.persist(&store.records.lock().unwrap())?;
        Ok(store)
    }

    fn persist(&self, journal: &Journal) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec(journal)?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.context("could not save task journal")
    }

    // Publish only after persistence succeeds, so an acknowledged result is retrievable.
    fn update<T>(&self, edit: impl FnOnce(&mut Journal) -> Result<T>) -> Result<T> {
        let mut current = self.records.lock().unwrap();
        let mut next = current.clone();
        let result = edit(&mut next)?;
        self.persist(&next)?;
        *current = next;
        self.changed.notify_all();
        Ok(result)
    }

    pub fn assign(&self, agent_id: AgentId, parent_id: AgentId, assignment: String) -> Result<()> {
        ensure!(!assignment.trim().is_empty(), "assignment cannot be empty");
        ensure!(
            assignment.len() <= crate::ipc::protocol::MAX_SPAWN_TASK_BYTES,
            "assignment is too large"
        );
        self.update(|journal| {
            ensure!(
                !journal.records.contains_key(&agent_id.0.to_string()),
                "session already has an assignment"
            );
            if journal.records.len() >= MAX_RECORDS {
                let oldest = journal
                    .records
                    .iter()
                    .filter(|(_, t)| t.archived)
                    .min_by_key(|(_, t)| t.updated_at)
                    .map(|(id, _)| id.clone());
                let Some(oldest) = oldest else {
                    bail!("task history is full; archive reviewed tasks first")
                };
                journal.records.remove(&oldest);
            }
            journal.records.insert(
                agent_id.0.to_string(),
                TaskRecord {
                    agent_id,
                    parent_id,
                    assignment,
                    state: TaskState::Assigned,
                    result: None,
                    archived: false,
                    updated_at: SystemTime::now(),
                },
            );
            Ok(())
        })
    }

    pub fn complete(&self, id: &AgentId, result: TaskResult) -> Result<TaskRecord> {
        ensure!(
            !result.summary.trim().is_empty(),
            "result summary cannot be empty"
        );
        ensure!(
            result.work_dir.is_absolute(),
            "work directory must be absolute"
        );
        ensure!(
            serde_json::to_vec(&result)?.len() <= MAX_RESULT_BYTES,
            "result exceeds 16 KiB"
        );
        self.update(|journal| {
            let task = journal
                .records
                .get_mut(&id.0.to_string())
                .context("unknown task")?;
            if task.state == TaskState::Complete && task.result.as_ref() == Some(&result) {
                return Ok(task.clone());
            }
            ensure!(task.state == TaskState::Assigned, "task is already closed");
            task.state = TaskState::Complete;
            task.result = Some(result);
            task.updated_at = SystemTime::now();
            Ok(task.clone())
        })
    }

    pub fn accept(&self, id: &AgentId) -> Result<TaskRecord> {
        self.update(|journal| {
            let task = journal
                .records
                .get_mut(&id.0.to_string())
                .context("unknown task")?;
            ensure!(
                matches!(task.state, TaskState::Complete | TaskState::Accepted),
                "only a completed result can be accepted"
            );
            task.state = TaskState::Accepted;
            task.updated_at = SystemTime::now();
            Ok(task.clone())
        })
    }

    pub fn archive(&self, id: &AgentId) -> Result<TaskRecord> {
        self.update(|journal| {
            let task = journal
                .records
                .get_mut(&id.0.to_string())
                .context("unknown task")?;
            ensure!(
                matches!(task.state, TaskState::Accepted | TaskState::Interrupted),
                "accept the result before archiving"
            );
            task.archived = true;
            task.updated_at = SystemTime::now();
            Ok(task.clone())
        })
    }

    pub fn interrupt(&self, ids: &[AgentId]) -> Result<()> {
        // Ordinary root drops have no task; avoid unnecessary journal writes.
        if !ids
            .iter()
            .any(|id| self.get(id).is_some_and(|t| t.state == TaskState::Assigned))
        {
            return Ok(());
        }
        self.update(|journal| {
            for id in ids {
                if let Some(task) = journal.records.get_mut(&id.0.to_string()) {
                    if task.state == TaskState::Assigned {
                        task.state = TaskState::Interrupted;
                        task.updated_at = SystemTime::now();
                    }
                }
            }
            Ok(())
        })
    }

    pub fn list(&self, parent: Option<&AgentId>, include_archived: bool) -> Vec<TaskRecord> {
        self.records
            .lock()
            .unwrap()
            .records
            .values()
            .filter(|task| {
                parent.is_none_or(|id| &task.parent_id == id)
                    && (include_archived || !task.archived)
            })
            .cloned()
            .collect()
    }

    /// Waits without polling. Returns the current record and whether the deadline expired.
    pub fn wait(&self, id: &AgentId, timeout: Duration) -> Result<(TaskRecord, bool)> {
        ensure!(
            timeout <= Duration::from_secs(60),
            "wait timeout must be at most 60 seconds"
        );
        let guard = self.records.lock().unwrap();
        ensure!(
            guard.records.contains_key(&id.0.to_string()),
            "unknown task"
        );
        let (guard, outcome) = self
            .changed
            .wait_timeout_while(guard, timeout, |journal| {
                journal
                    .records
                    .get(&id.0.to_string())
                    .is_some_and(|t| t.state == TaskState::Assigned)
            })
            .unwrap();
        let task = guard
            .records
            .get(&id.0.to_string())
            .context("task was pruned")?
            .clone();
        let timed_out = outcome.timed_out() && task.state == TaskState::Assigned;
        Ok((task, timed_out))
    }

    pub fn get(&self, id: &AgentId) -> Option<TaskRecord> {
        self.records
            .lock()
            .unwrap()
            .records
            .get(&id.0.to_string())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_is_retained_independently_of_session_state() {
        let store = TaskStore::default();
        let id = AgentId::new();
        let parent = AgentId::new();
        store
            .assign(
                id.clone(),
                parent.clone(),
                "full assignment, not its label".into(),
            )
            .unwrap();
        let task = store.get(&id).expect("assignment must remain queryable");
        assert_eq!(task.assignment, "full assignment, not its label");
        assert_eq!(task.parent_id, parent);
        assert_eq!(task.state, TaskState::Assigned);
    }
    fn result() -> TaskResult {
        TaskResult {
            summary: "Fixed and verified".into(),
            work_dir: PathBuf::from("/tmp/repo"),
            artifacts: vec!["commit:abc123".into()],
            validation: "cargo test passed".into(),
        }
    }

    #[test]
    fn results_survive_restart_and_active_tasks_become_interrupted() {
        let dir = std::env::temp_dir().join(format!("ovsr-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("tasks.json");
        let completed = AgentId::new();
        let pending = AgentId::new();
        let parent = AgentId::new();
        {
            let store = TaskStore::open(&path).unwrap();
            store
                .assign(completed.clone(), parent.clone(), "first".into())
                .unwrap();
            store
                .assign(pending.clone(), parent.clone(), "second".into())
                .unwrap();
            store.complete(&completed, result()).unwrap();
        }
        let store = TaskStore::open(&path).unwrap();
        assert_eq!(store.get(&completed).unwrap().result, Some(result()));
        assert_eq!(store.get(&pending).unwrap().state, TaskState::Interrupted);
        assert_eq!(store.list(Some(&parent), false).len(), 2);
        store.interrupt(&[completed.clone()]).unwrap();
        assert_eq!(store.get(&completed).unwrap().state, TaskState::Complete);
        assert!(store.archive(&completed).is_err());
        store.accept(&completed).unwrap();
        store.archive(&completed).unwrap();
        assert_eq!(store.list(Some(&parent), false).len(), 1);
        assert_eq!(store.list(Some(&parent), true).len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wait_wakes_for_a_result_and_has_a_bounded_timeout() {
        let store = std::sync::Arc::new(TaskStore::default());
        let id = AgentId::new();
        store
            .assign(id.clone(), AgentId::new(), "work".into())
            .unwrap();
        assert!(store.wait(&id, Duration::ZERO).unwrap().1);
        let waiting = store.clone();
        let waiting_id = id.clone();
        let thread =
            std::thread::spawn(move || waiting.wait(&waiting_id, Duration::from_secs(2)).unwrap());
        store.complete(&id, result()).unwrap();
        let (record, timed_out) = thread.join().unwrap();
        assert!(!timed_out);
        assert_eq!(record.result, Some(result()));
        assert!(store.wait(&id, Duration::from_secs(61)).is_err());
    }

    #[test]
    fn failed_persistence_does_not_acknowledge_a_result() {
        let dir = std::env::temp_dir().join(format!("ovsr-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let store = TaskStore::open(&dir.join("tasks.json")).unwrap();
        let id = AgentId::new();
        store
            .assign(id.clone(), AgentId::new(), "work".into())
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(store.complete(&id, result()).is_err());
        assert_eq!(store.get(&id).unwrap().state, TaskState::Assigned);
    }

    #[test]
    fn corrupt_journal_is_preserved_and_completion_is_idempotent() {
        let path = std::env::temp_dir().join(format!("ovsr-tasks-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, "not json").unwrap();
        assert!(TaskStore::open(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        std::fs::remove_file(path).unwrap();
        let store = TaskStore::default();
        let id = AgentId::new();
        store
            .assign(id.clone(), AgentId::new(), "work".into())
            .unwrap();
        store.complete(&id, result()).unwrap();
        store.complete(&id, result()).unwrap();
        let mut different = result();
        different.summary = "replacement".into();
        assert!(store.complete(&id, different).is_err());
        assert!(store
            .assign(id, AgentId::new(), "replacement".into())
            .is_err());
    }
}
