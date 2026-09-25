use super::ContextualUserFragment;

pub(crate) struct WorkingMemoryInstructions(pub String);

impl ContextualUserFragment for WorkingMemoryInstructions {
    fn content_kind(&self) -> codex_protocol::models::ContentItemKind {
        codex_protocol::models::ContentItemKind("working_memory.instructions".to_string())
    }
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<working_memory>", "</working_memory>")
    }
    fn body(&self) -> String {
        format!(
            "History is bounded. The turn index is only a summary; use history_read with turn IDs (T001, etc.) for missing details. Never invent missing context or repeat completed work solely because it is outside the window.\n{}",
            self.0
        )
    }
}
