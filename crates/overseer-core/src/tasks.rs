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
const MAX_JOURNAL_BYTES: usize = 8 * 1024 * 1024;
// Timestamp precision and state names can grow slightly during later transitions.
const RECORD_METADATA_RESERVE: usize = 64;

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

impl From<&TaskRecord> for TaskSummary {
    fn from(task: &TaskRecord) -> Self {
        let text = task
            .result
            .as_ref()
            .map(|r| r.summary.as_str())
            .unwrap_or(&task.assignment);
        Self {
            agent_id: task.agent_id.clone(),
            parent_id: task.parent_id.clone(),
            state: task.state.clone(),
            summary: text.chars().take(120).collect(),
            archived: task.archived,
        }
    }
}

impl From<TaskRecord> for TaskSummary {
    fn from(task: TaskRecord) -> Self {
        Self::from(&task)
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
        use std::io::Read;
        let mut records: Journal = match std::fs::File::open(path) {
            Ok(file) => {
                // Bound the read itself, including files that grow after open.
                let mut bytes = Vec::new();
                file.take((MAX_JOURNAL_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= MAX_JOURNAL_BYTES,
                    "task journal exceeds 8 MiB; existing file was preserved"
                );
                serde_json::from_slice(&bytes)
                    .context("invalid task journal; existing file was preserved")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            records.records.len() <= MAX_RECORDS,
            "task journal exceeds 512 records; existing file was preserved"
        );
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
        let encoded = serde_json::to_vec(journal)?;
        ensure!(
            encoded.len() <= MAX_JOURNAL_BYTES,
            "task journal exceeds 8 MiB"
        );
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
            file.write_all(&encoded)?;
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
            // Reserve each open task's maximum result now, so a full inbox
            // cannot prevent reporting completion later. Include JSON escaping
            // in the budget, not just the assignment's UTF-8 byte length.
            let mut usage = serde_json::to_vec(journal)?.len()
                + journal.records.values().map(Self::reserved_bytes).sum::<usize>();
            while journal.records.len() > MAX_RECORDS || usage > MAX_JOURNAL_BYTES {
                let oldest = journal.records.iter()
                    .filter(|(_, task)| task.archived)
                    .min_by_key(|(_, task)| task.updated_at)
                    .map(|(id, _)| id.clone());
                let Some(oldest) = oldest else {
                    bail!("task history is full (512 records or 8 MiB including reserved results); archive reviewed tasks first")
                };
                let removed = journal.records.remove(&oldest).unwrap();
                // At least the new unarchived record remains: removing this
                // object member also removes exactly one colon and comma.
                usage -= serde_json::to_vec(&oldest)?.len() + serde_json::to_vec(&removed)?.len()
                    + 2 + Self::reserved_bytes(&removed);
            }
            Ok(())
        })
    }

    fn reserved_bytes(task: &TaskRecord) -> usize {
        RECORD_METADATA_RESERVE
            + if task.state == TaskState::Assigned {
                MAX_RESULT_BYTES + RECORD_METADATA_RESERVE
            } else {
                0
            }
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
        let mut current = self.records.lock().unwrap();
        if !ids.iter().any(|id| {
            current
                .records
                .get(&id.0.to_string())
                .is_some_and(|task| task.state == TaskState::Assigned)
        }) {
            return Ok(());
        }
        let mut next = current.clone();
        for id in ids {
            if let Some(task) = next.records.get_mut(&id.0.to_string()) {
                if task.state == TaskState::Assigned {
                    task.state = TaskState::Interrupted;
                    task.updated_at = SystemTime::now();
                }
            }
        }
        // A process exit cannot be rolled back when storage fails. Publish the
        // truth and wake waiters anyway; return the error for the daemon log.
        // A later successful mutation persists this state along with its own.
        let persisted = self.persist(&next);
        *current = next;
        self.changed.notify_all();
        persisted
    }

    /// Projects only inbox fields; large assignments/results are never cloned.
    pub fn summaries(&self, parent: Option<&AgentId>, include_archived: bool) -> Vec<TaskSummary> {
        self.records
            .lock()
            .unwrap()
            .records
            .values()
            .filter(|task| {
                parent.is_none_or(|id| &task.parent_id == id)
                    && (include_archived || !task.archived)
            })
            .map(TaskSummary::from)
            .collect()
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
    fn interrupted_state_wakes_waiters_even_when_persistence_fails() {
        let dir = std::env::temp_dir().join(format!("ovsr-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("tasks.json");
        let store = std::sync::Arc::new(TaskStore::open(&path).unwrap());
        let id = AgentId::new();
        store
            .assign(id.clone(), AgentId::new(), "work".into())
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let waiting = store.clone();
        let waiting_id = id.clone();
        let waiter =
            std::thread::spawn(move || waiting.wait(&waiting_id, Duration::from_secs(2)).unwrap());
        assert!(store.interrupt(std::slice::from_ref(&id)).is_err());
        let (record, timed_out) = waiter.join().unwrap();
        assert!(!timed_out);
        assert_eq!(record.state, TaskState::Interrupted);
        std::fs::create_dir(&dir).unwrap();
        store
            .assign(AgentId::new(), AgentId::new(), "next work".into())
            .unwrap();
        assert_eq!(
            TaskStore::open(&path).unwrap().get(&id).unwrap().state,
            TaskState::Interrupted
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn serialized_budget_includes_escaping_and_reserves_all_completion_results() {
        let store = TaskStore::default();
        let assignment = "\u{0001}".repeat(crate::ipc::protocol::MAX_SPAWN_TASK_BYTES);
        let mut admitted = Vec::new();
        for _ in 0..16 {
            let id = AgentId::new();
            if store
                .assign(id.clone(), AgentId::new(), assignment.clone())
                .is_err()
            {
                assert!(
                    store.get(&id).is_none(),
                    "rejected assignment must roll back"
                );
                break;
            }
            admitted.push(id);
        }
        assert!(
            !admitted.is_empty() && admitted.len() < 16,
            "escaped JSON must hit the byte budget first"
        );
        let mut largest = result();
        largest.summary.clear();
        largest.summary =
            "x".repeat(MAX_RESULT_BYTES - serde_json::to_vec(&largest).unwrap().len());
        assert_eq!(
            serde_json::to_vec(&largest).unwrap().len(),
            MAX_RESULT_BYTES
        );
        for id in &admitted {
            store
                .complete(id, largest.clone())
                .expect("capacity must not prevent an admitted task completing");
        }
        assert!(
            serde_json::to_vec(&*store.records.lock().unwrap())
                .unwrap()
                .len()
                <= MAX_JOURNAL_BYTES
        );
        let oldest = &admitted[0];
        store.accept(oldest).unwrap();
        store.archive(oldest).unwrap();
        store
            .assign(AgentId::new(), AgentId::new(), assignment)
            .unwrap();
        assert!(
            store.get(oldest).is_none(),
            "archived entries also make room under the byte cap"
        );
        assert!(admitted[1..].iter().all(|id| store.get(id).is_some()));
    }

    fn closed_record(id: AgentId) -> TaskRecord {
        TaskRecord {
            agent_id: id,
            parent_id: AgentId::new(),
            assignment: "work".into(),
            state: TaskState::Interrupted,
            result: None,
            archived: false,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn full_history_prunes_oldest_archived_only_and_rolls_back_on_save_failure() {
        let mut journal = Journal::default();
        let oldest = AgentId::new();
        let newer = AgentId::new();
        for id in [oldest.clone(), newer.clone()]
            .into_iter()
            .chain((2..MAX_RECORDS).map(|_| AgentId::new()))
        {
            let mut record = closed_record(id.clone());
            record.archived = id == oldest || id == newer;
            if id == newer {
                record.updated_at += Duration::from_secs(1);
            }
            journal.records.insert(id.0.to_string(), record);
        }
        let dir = std::env::temp_dir().join(format!("ovsr-tasks-missing-{}", uuid::Uuid::new_v4()));
        let mut store = TaskStore {
            records: Mutex::new(journal),
            path: Some(dir.join("tasks.json")),
            ..Default::default()
        };
        assert!(store
            .assign(AgentId::new(), AgentId::new(), "new".into())
            .is_err());
        assert!(
            store.get(&oldest).is_some(),
            "failed persistence must not discard archived history"
        );
        store.path = None;
        store
            .assign(AgentId::new(), AgentId::new(), "new".into())
            .unwrap();
        assert!(store.get(&oldest).is_none());
        assert!(store.get(&newer).is_some());
        assert_eq!(store.summaries(None, true).len(), MAX_RECORDS);
    }

    #[test]
    fn oversized_journals_are_rejected_without_rewriting() {
        let path = std::env::temp_dir().join(format!("ovsr-tasks-{}.json", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_JOURNAL_BYTES + 1) as u64).unwrap();
        drop(file);
        assert!(TaskStore::open(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (MAX_JOURNAL_BYTES + 1) as u64
        );
        let mut journal = Journal::default();
        for _ in 0..=MAX_RECORDS {
            let id = AgentId::new();
            journal.records.insert(id.0.to_string(), closed_record(id));
        }
        let encoded = serde_json::to_vec(&journal).unwrap();
        std::fs::write(&path, &encoded).unwrap();
        assert!(TaskStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), encoded);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn summaries_filter_parent_archive_and_project_bounded_unicode_text() {
        let store = TaskStore::default();
        let id = AgentId::new();
        let parent = AgentId::new();
        store
            .assign(id.clone(), parent.clone(), "🌲".repeat(1000))
            .unwrap();
        store
            .assign(AgentId::new(), AgentId::new(), "other team".into())
            .unwrap();
        let summaries = store.summaries(Some(&parent), false);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary, "🌲".repeat(120));
        store.complete(&id, result()).unwrap();
        assert_eq!(
            store.summaries(Some(&parent), false)[0].summary,
            result().summary
        );
        store.accept(&id).unwrap();
        store.archive(&id).unwrap();
        assert!(store.summaries(Some(&parent), false).is_empty());
        assert_eq!(store.summaries(Some(&parent), true).len(), 1);
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
