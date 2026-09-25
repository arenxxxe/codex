use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::working_memory::Settings;
use crate::working_memory::build_snapshot;
use crate::working_memory::history;
use crate::working_memory::parse_turn_id;
use crate::working_memory::turn_id;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

pub(crate) struct WorkingMemoryHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    start_turn: Option<String>,
    end_turn: Option<String>,
}

impl ToolExecutor<ToolInvocation> for WorkingMemoryHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("history_read")
    }

    #[expect(clippy::expect_used, reason = "Static tool schema")]
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "history_read".to_string(),
            description: "Read persisted conversation history by turn ID (T001). No arguments returns the turn count; a range returns at most 50 turns and 24000 bytes. Follow next_start_turn to continue.".to_string(),
            strict: false,
            defer_loading: None,
            output_schema: None,
            parameters: serde_json::from_value(json!({
                "type": "object",
                "properties": {
                    "start_turn": {"type": "string", "description": "First turn ID, inclusive."},
                    "end_turn": {"type": "string", "description": "Last turn ID, inclusive; defaults to start_turn."}
                },
                "required": [],
                "additionalProperties": false
            }))
            .expect("static history_read schema"),
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "Expected function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            let result = self.run(&invocation, args).await.map_err(|err| {
                FunctionCallError::RespondToModel(format!("Working memory: {err:#}"))
            })?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for WorkingMemoryHandler {}

impl WorkingMemoryHandler {
    async fn run(&self, invocation: &ToolInvocation, args: Args) -> anyhow::Result<String> {
        anyhow::ensure!(
            Settings::load(&invocation.turn.config.codex_home)?.enabled,
            "Working memory is disabled"
        );
        let records = history(&invocation.session).await?;
        let entries = build_snapshot(&records);
        if args.start_turn.is_none() && args.end_turn.is_none() {
            return Ok(json!({
                "total_turns": entries.len(),
                "latest_turn_id": (!entries.is_empty()).then(|| turn_id(entries.len()))
            })
            .to_string());
        }
        let start = parse_turn_id(args.start_turn.as_deref().unwrap_or("T001"))?;
        let end = args
            .end_turn
            .as_deref()
            .map(parse_turn_id)
            .transpose()?
            .unwrap_or(start);
        anyhow::ensure!(start <= end, "start_turn must not be after end_turn");
        anyhow::ensure!(
            end <= entries.len(),
            "Latest turn is {}",
            turn_id(entries.len())
        );

        let mut turns = Vec::new();
        let mut bytes = 0;
        for number in start..=end.min(start.saturating_add(49)) {
            let entry = &entries[number - 1];
            let turn = json!({
                "id": turn_id(number),
                "messages": &records[entry.start - 1..entry.end]
            });
            let size = serde_json::to_vec(&turn)?.len();
            if bytes + size > 24_000 {
                break;
            }
            bytes += size;
            turns.push(turn);
        }
        anyhow::ensure!(
            !turns.is_empty(),
            "Turn exceeds the 24000-byte output limit"
        );
        let next = start + turns.len();
        Ok(json!({
            "turns": turns,
            "next_start_turn": (next <= end).then(|| turn_id(next))
        })
        .to_string())
    }
}
