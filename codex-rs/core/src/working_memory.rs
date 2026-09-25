//! Bounded request history: a sliding window over the newest items plus an
//! always-visible turn index. The full rollout stays on disk for on-demand
//! reads through the `history_read` tool.

use crate::client_common::Prompt;
use crate::context::ContextualUserFragment;
use crate::context::WorkingMemoryInstructions;
use crate::context::is_contextual_user_fragment;
use crate::context::is_user_authorization_message;
use crate::session::session::Session;
use codex_history::RolloutItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutRecorder;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub history_token_budget: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            history_token_budget: 20_000,
        }
    }
}

impl Settings {
    pub(crate) fn load(codex_home: &Path) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let path = Path::new(r"C:\settings.json");
        #[cfg(not(windows))]
        let path = Path::new("/etc/codex/settings.json");
        let path = if path.exists() {
            path.to_path_buf()
        } else {
            codex_home.join("settings.json")
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let Some(value) = value.get("codex_memory") else {
            return Ok(Self::default());
        };
        let settings: Self = serde_json::from_value(value.clone())?;
        anyhow::ensure!(
            (1_000..=1_000_000).contains(&settings.history_token_budget),
            "history_token_budget must be between 1000 and 1000000"
        );
        Ok(settings)
    }
}

pub(crate) async fn history(session: &Session) -> anyhow::Result<Vec<ResponseItem>> {
    session.flush_rollout().await?;
    let path = session
        .current_rollout_path()
        .await?
        .ok_or_else(|| anyhow::anyhow!("Working memory requires a local rollout"))?;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await?;
    anyhow::ensure!(errors == 0, "Rollout has {errors} malformed records");
    Ok(items
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(envelope.item),
            _ => None,
        })
        .collect())
}

pub(crate) fn is_instruction(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, content, .. } => {
            role == "system"
                || role == "developer"
                || (role == "user"
                    && !content.is_empty()
                    && content.iter().all(is_contextual_user_fragment))
        }
        ResponseItem::AdditionalTools { .. } | ResponseItem::ConfigurationUpdate { .. } => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Turn index: one mechanical line per turn, always visible in the prompt.
// ---------------------------------------------------------------------------

pub(crate) struct IndexEntry {
    pub(crate) start: usize,
    pub(crate) end: usize,
    user: String,
    tools: Vec<(String, usize)>,
    answer: String,
}

pub(crate) fn turn_id(number: usize) -> String {
    format!("T{number:03}")
}

pub(crate) fn parse_turn_id(value: &str) -> anyhow::Result<usize> {
    let number = value
        .strip_prefix('T')
        .ok_or_else(|| anyhow::anyhow!("Turn IDs must look like T001"))?
        .parse::<usize>()?;
    anyhow::ensure!(number > 0, "Turn IDs must be positive");
    Ok(number)
}

pub(crate) fn build_snapshot(items: &[ResponseItem]) -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let ordinal = index + 1;
        if is_user_authorization_message(item) && !is_instruction(item) {
            let ResponseItem::Message { content, .. } = item else {
                continue;
            };
            entries.push(IndexEntry {
                start: ordinal,
                end: ordinal,
                user: summary(content, "user", 80),
                tools: Vec::new(),
                answer: String::new(),
            });
        } else if let Some(entry) = entries.last_mut() {
            entry.end = ordinal;
            if let Some(tool) = tool_summary(item) {
                match entry.tools.iter().position(|(name, _)| *name == tool) {
                    Some(position) => entry.tools[position].1 += 1,
                    None => entry.tools.push((tool, 1)),
                }
            }
            if let ResponseItem::Message { role, content, .. } = item
                && role == "assistant"
            {
                let answer = summary(content, "assistant", 120);
                if !answer.is_empty() {
                    entry.answer = answer;
                }
            }
        }
    }
    entries
}

fn tool_summary(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => Some(match command_hint(arguments) {
            Some(hint) => format!("{name}({hint})"),
            None => name.clone(),
        }),
        ResponseItem::CustomToolCall { name, .. } => Some(name.clone()),
        ResponseItem::LocalShellCall { action, .. } => match action {
            LocalShellAction::Exec(exec) => {
                Some(format!("shell({})", truncate(&exec.command.join(" "), 40)))
            }
        },
        _ => None,
    }
}

fn command_hint(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    for key in ["cmd", "command", "script"] {
        if let Some(text) = value.get(key).and_then(|value| value.as_str()) {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                return Some(truncate(&text, 40));
            }
        }
    }
    None
}

fn summary(content: &[ContentItem], role: &str, cap: usize) -> String {
    let text = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } if role == "user" => Some(text.as_str()),
            ContentItem::OutputText { text } if role == "assistant" => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), cap)
}

fn truncate(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn render(entries: &[IndexEntry]) -> String {
    const MAX_LINES: usize = 200;
    const MAX_BYTES: usize = 16_000;
    let mut output = format!("indexed_turns={} (newest first)\n", entries.len());
    let mut lowest_rendered = entries.len() + 1;
    for (index, entry) in entries.iter().enumerate().rev().take(MAX_LINES) {
        let line = index_line(index + 1, entry);
        if output.len() + line.len() > MAX_BYTES {
            break;
        }
        output.push_str(&line);
        lowest_rendered = index + 1;
    }
    if lowest_rendered > 1 {
        let last = lowest_rendered - 1;
        let range = if last == 1 {
            turn_id(1)
        } else {
            format!("{}-{}", turn_id(1), turn_id(last))
        };
        output.push_str(&format!("{range} (older; use history_read)\n"));
    }
    output
}

fn index_line(number: usize, entry: &IndexEntry) -> String {
    let tools = if entry.tools.is_empty() {
        String::new()
    } else {
        let list = entry
            .tools
            .iter()
            .map(|(name, count)| {
                if *count == 1 {
                    name.clone()
                } else {
                    format!("{name}x{count}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("; tools: {list}")
    };
    let user = if entry.user.is_empty() {
        "-"
    } else {
        entry.user.as_str()
    };
    let answer = if entry.answer.is_empty() {
        "-"
    } else {
        entry.answer.as_str()
    };
    format!(
        "{} user: {user}{tools}; answer: {answer}\n",
        turn_id(number)
    )
}

// ---------------------------------------------------------------------------
// Sliding window: keep the newest non-protected items within the token budget.
// ---------------------------------------------------------------------------

fn remove_compaction(input: &mut Vec<ResponseItem>) {
    input.retain(|item| {
        !matches!(
            item,
            ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction { .. }
                | ResponseItem::CompactionTrigger { .. }
        )
    });
}

fn protected_items(input: &[ResponseItem]) -> Vec<bool> {
    let latest_user = input
        .iter()
        .rposition(|item| is_user_authorization_message(item) && !is_instruction(item));
    input
        .iter()
        .enumerate()
        .map(|(index, item)| Some(index) == latest_user || is_instruction(item))
        .collect()
}

fn historical_items<'a>(
    input: &'a [ResponseItem],
    protected: &[bool],
) -> Vec<(usize, &'a ResponseItem)> {
    input
        .iter()
        .enumerate()
        .filter(|(index, _)| !protected[*index])
        .collect()
}

fn history_cutoff(items: &[(usize, &ResponseItem)], budget: usize) -> anyhow::Result<usize> {
    let tokenizer = tiktoken_rs::o200k_base_singleton();
    let mut used = 0;
    let mut first_kept = items.last().map_or(0, |(index, _)| index + 1);
    for (index, item) in items.iter().rev() {
        let serialized = serde_json::to_string(item)?;
        let tokens = tokenizer.count_ordinary(&serialized).max(1);
        if tokens > budget - used {
            break;
        }
        used += tokens;
        first_kept = *index;
    }
    Ok(first_kept)
}

fn insert_index(prompt: &mut Prompt, history: &[ResponseItem]) {
    let snapshot = build_snapshot(history);
    let position = prompt
        .input
        .iter()
        .rposition(|item| is_user_authorization_message(item) && !is_instruction(item))
        .map_or(prompt.input.len(), |index| index + 1);
    prompt.input.insert(
        position,
        ContextualUserFragment::into(WorkingMemoryInstructions(render(&snapshot))),
    );
}

pub(crate) async fn prepare(
    session: &Session,
    codex_home: &Path,
    mut prompt: Prompt,
) -> anyhow::Result<Prompt> {
    let settings = Settings::load(codex_home)?;
    if !settings.enabled {
        return Ok(prompt);
    }
    remove_compaction(&mut prompt.input);
    let complete_history = history(session).await?;
    let protected = protected_items(&prompt.input);
    let pure_history = historical_items(&prompt.input, &protected);
    let first_kept = history_cutoff(&pure_history, settings.history_token_budget as usize)?;
    let mut position = 0;
    prompt.input.retain(|_| {
        let keep = protected[position] || position >= first_kept;
        position += 1;
        keep;
    });
    insert_index(&mut prompt, &complete_history);
    Ok(prompt)
}
