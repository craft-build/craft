use craft_providers::{ContentBlock, Message, Role};

use super::super::history::remove_orphaned_tool_results;

use super::{KEEP_LAST_TOOL_RESULTS, TOOL_RESULT_PLACEHOLDER};

pub(super) fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            match block {
                ContentBlock::Image { .. } => {
                    *block = ContentBlock::Text {
                        text: super::IMAGE_PLACEHOLDER.into(),
                    };
                }
                ContentBlock::ToolResult { images, .. } if !images.is_empty() => {
                    images.clear();
                }
                _ => {}
            }
        }
    }
}

pub(super) fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| !block.is_thinking());
    }
}

pub(super) fn strip_old_tool_results(messages: &mut [Message]) {
    let total: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();

    let mut seen = 0;
    for msg in messages {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                if seen < total.saturating_sub(KEEP_LAST_TOOL_RESULTS) {
                    *content = TOOL_RESULT_PLACEHOLDER.into();
                }
                seen += 1;
            }
        }
    }
}

pub(super) fn strip_tool_results_by_ratio(messages: &mut [Message], ratio: f32) -> usize {
    let mut indices: Vec<(usize, usize)> = Vec::new();
    for (mi, m) in messages.iter().enumerate() {
        for (bi, b) in m.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = b
                && content.as_str() != TOOL_RESULT_PLACEHOLDER
            {
                indices.push((mi, bi));
            }
        }
    }
    let total = indices.len();
    if total == 0 {
        return 0;
    }
    let target = (total as f32 * ratio).ceil() as usize;
    let mut dropped = 0;
    for (mi, bi) in indices.into_iter().take(target) {
        if let ContentBlock::ToolResult { content, .. } = &mut messages[mi].content[bi]
            && content.as_str() != TOOL_RESULT_PLACEHOLDER
        {
            *content = TOOL_RESULT_PLACEHOLDER.into();
            dropped += 1;
        }
    }
    dropped
}

pub(super) fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let removed_user = matches!(messages.remove(0).role, Role::User);
    if removed_user
        && messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
    }
    remove_orphaned_tool_results(messages);

    while messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
        remove_orphaned_tool_results(messages);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::assert_tool_results_have_calls;
    use super::*;

    #[test]
    fn strip_old_tool_results_keeps_newest() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result 1".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old result 2".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep 1".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t4".into(),
                    content: "keep 2".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t5".into(),
                    content: "keep 3".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert_eq!(messages[0].content.len(), 6);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t1")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t2")
        );
        assert!(
            matches!(&messages[0].content[2], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 1" && tool_use_id == "t3")
        );
        assert!(
            matches!(&messages[0].content[3], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 2" && tool_use_id == "t4")
        );
        assert!(
            matches!(&messages[0].content[4], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 3" && tool_use_id == "t5")
        );
        assert!(
            matches!(&messages[0].content[5], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn strip_old_tool_results_keeps_all_when_fewer_than_threshold() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "only result".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "second".into(),
                    images: vec![],
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, .. } if content == "only result")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, .. } if content == "second")
        );
    }

    #[test]
    fn strip_tool_results_by_ratio_removes_oldest_first() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old1".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old2".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep".into(),
                    images: vec![],
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 0.5);
        assert_eq!(dropped, 2);
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
        assert!(matches!(
            &messages[0].content[2],
            ContentBlock::ToolResult { content, .. } if content == "keep"
        ));
    }

    #[test]
    fn strip_tool_results_by_ratio_full_removes_all() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "a".into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "b".into(),
                    images: vec![],
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 1.0);
        assert_eq!(dropped, 2);
        assert!(messages[0].content.iter().all(|b| matches!(
            b,
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        )));
    }

    #[test]
    fn strip_tool_results_by_ratio_skips_already_placeholder() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: TOOL_RESULT_PLACEHOLDER.into(),
                    images: vec![],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "real".into(),
                    images: vec![],
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 1.0);
        assert_eq!(dropped, 1);
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use craft_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == super::super::IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_thinking_removes_thinking_blocks() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn truncate_oldest_round_removes_single_user_message() {
        let mut messages = vec![
            Message::user("first".into()),
            Message::user("second".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "second"));
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_tool_pair() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_without_matching_tool_result() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message::user("no tool result".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "no tool result")
        );
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![Message::user("only".into())];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_plain_assistant() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "plain reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == "keep me")
        );
    }
}
