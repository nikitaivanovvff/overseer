//! One role contract shared by every harness. No model-side role filtering.
use crate::agent::AgentRole;
use crate::ipc::protocol::AgentDto;
use crate::tasks::TaskRecord;

pub const CONTRACT_VERSION: u32 = 1;

pub fn context(agent: &AgentDto, depth: usize, task: Option<&TaskRecord>) -> String {
    let identity = match agent.role {
        AgentRole::Root => "workspace agent",
        AgentRole::Child => "assigned contributor",
    };
    let mut text = format!(
        "Overseer integration v{CONTRACT_VERSION}. You are the {identity}, agent {}, at depth {depth}/3. Follow the user's authorization and repository instructions.\n",
        agent.id.0,
    );
    if depth < 3 {
        text.push_str("You are the team lead for any children you spawn. Delegate authorized, independently steerable tasks with `overseer spawn --name <label> --task <self-contained assignment> [--adapter claude|opencode|codex]`. Local parallel tool calls are allowed. Native subagents do not appear in Overseer's managed session tree.\n");
        text.push_str("Review your direct children's inbox with `overseer tasks`. Use `overseer wait <id> --timeout 30` for bounded waiting and `overseer task <id>` for the full assignment and result. Inspect the reported artifacts and validation, then `overseer accept <id>` and `overseer archive <id>`. Acceptance does not merge code; archive does not kill sessions or delete files.\n");
    } else {
        text.push_str("You are a leaf contributor. The runtime rejects further child spawning; perform this assignment inline.\n");
    }
    if let Some(task) = task {
        text.push_str(&format!("Your retained assignment is available through `overseer task {}` (task state: {:?}). When it is complete, run `overseer complete --summary <summary> --artifact <path-or-commit> --validation <checks-and-results>` from your actual work directory. Repeat --artifact as needed. Reported completion awaits review and is separate from runtime activity.\n", task.agent_id.0, task.state));
    } else if agent.role == AgentRole::Child {
        text.push_str("No retained assignment is registered yet. Wait for the user's task, then record it with `overseer assign <your-id> --task <assignment>` before working. This records metadata and does not type into a terminal.\n");
    }
    text.push_str("Choose isolation appropriate to the assignment: separate writable work when needed; read-only and non-git tasks need no worktree. Do not overwrite another agent's changes. Use the actual task base when reviewing commits. Before cleanup, stop the session, inspect tracked and untracked changes, and preserve uncertain work. Never assume forced worktree deletion is safe.\nRuntime status comes from integration hooks; do not report task success merely because a session is idle or has exited. An idle parent is not automatically awakened: wait explicitly or return control to the user.\n");
    text
}
