//! The summarize request shape and the surface checkpoint message
//! (compaction-basic/summarizer.ts parity, verbatim instruction).

use starriver_kernel::llm::{mint_message_id, ContentBlock, Message, MessageRole, MessageSource};

/// summarization instruction (verbatim from compaction-basic/summarizer.ts).
pub const COMPACTION_INSTRUCTION: &str = r#"
You are now acting as a compaction engine for this AI coding assistant.
Condense the conversation ABOVE into a structured checkpoint that lets
another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order.
Use terse bullets, not prose paragraphs. Write "(none)" for an empty section
— never drop a section.

## Primary Request and Intent
## Key Technical Concepts
## Files and Code
## Errors and Fixes
## Pending Jobs
## Current Work
## Next Step
## Critical Context

Rules:
- Write concise English engineering prose.
- Capture user feedback and explicit instructions faithfully.
- Do NOT mention this summarization request.
- If the conversation already contains a <compacted-summary> block, merge
  newer information into it rather than copying it forward.
"#;

/// checkpoint preamble (verbatim from compaction-basic/summarizer.ts).
pub const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// The trailing user message of a summarize call (summarizer parity: the
/// instruction is appended after the live surface, keeping the request's
/// prefix identical to the conversation's own requests).
pub fn summarize_request_message() -> Message {
    Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text(COMPACTION_INSTRUCTION.to_string())],
        source: MessageSource::Plugin {
            plugin: "compaction".into(),
            form: None,
        },
    }
}

/// The checkpoint written to the surface: preamble + the summary wrapped
/// in `<compacted-summary>` (prior checkpoints merge per the instruction).
pub fn checkpoint_message(summary: &str) -> Message {
    Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text(format!(
            "{CHECKPOINT_PREAMBLE}\n\n<compacted-summary>\n{summary}\n</compacted-summary>"
        ))],
        source: MessageSource::Plugin {
            plugin: "compaction".into(),
            form: None,
        },
    }
}

/// BUG-127: collect the distinct `sha256:` file ids carried by Image blocks
/// in the range about to be shadowed — without this the checkpoint's text
/// summary drops the images' identity entirely and `reload_image` has no
/// handle left after compaction.
pub fn compacted_image_manifest(messages: &[Message]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for message in messages {
        for block in &message.content {
            let ContentBlock::Image { attachment } = block else {
                continue;
            };
            if let Some(id) = attachment
                .as_str_at("fileId")
                .filter(|id| id.starts_with("sha256:"))
            {
                if !ids.iter().any(|seen| seen == id) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

/// BUG-127: carry the manifest on the checkpoint so the model keeps a
/// recovery path for compacted images (reload the listed file ids).
pub fn append_image_manifest(checkpoint: &mut Message, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let shown = ids.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
    let more = (ids.len() > 8)
        .then(|| format!(" …(共 {} 张)", ids.len()))
        .unwrap_or_default();
    checkpoint.content.push(ContentBlock::Text(format!(
        "[已压缩段包含 {} 张历史图片;如需查看,调用 reload_image 并传对应 file_id:{shown}{more}]",
        ids.len()
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{MessageId, MessageRole};

    fn image_message(file_id: &str) -> Message {
        Message {
            id: MessageId(format!("m-{file_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::Image {
                attachment: JsonValue::Object(vec![(
                    "fileId".into(),
                    JsonValue::String(file_id.to_string()),
                )]),
            }],
            source: MessageSource::User,
        }
    }

    fn text_message() -> Message {
        Message {
            id: MessageId("m-text".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("no images".into())],
            source: MessageSource::User,
        }
    }

    /// BUG-127: the manifest keeps every distinct compacted image id (same
    /// id pasted twice lists once) and rides on the checkpoint text so
    /// reload_image keeps a handle after compaction.
    #[test]
    fn manifest_collects_distinct_ids_and_rides_the_checkpoint() {
        let messages = vec![
            text_message(),
            image_message("sha256:aa"),
            image_message("sha256:aa"),
            image_message("sha256:bb"),
        ];
        let ids = compacted_image_manifest(&messages);
        assert_eq!(ids, vec!["sha256:aa".to_string(), "sha256:bb".to_string()]);

        let mut checkpoint = checkpoint_message("summary");
        append_image_manifest(&mut checkpoint, &ids);
        assert_eq!(checkpoint.content.len(), 2, "summary + manifest line");
        let ContentBlock::Text(manifest) = &checkpoint.content[1] else {
            panic!("manifest is text");
        };
        assert!(manifest.contains("2 张历史图片"), "{manifest}");
        assert!(manifest.contains("sha256:aa"), "{manifest}");
        assert!(manifest.contains("sha256:bb"), "{manifest}");
        assert!(manifest.contains("reload_image"), "{manifest}");

        // No images → no manifest line at all.
        let mut bare = checkpoint_message("summary");
        append_image_manifest(&mut bare, &[]);
        assert_eq!(bare.content.len(), 1);
    }
}
