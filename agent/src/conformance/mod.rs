//! Offline provider conformance: every supported
//! config kind drives a real [`Conversation`] against a local mock HTTP
//! server speaking its protocol, proving text, thinking and tool-call round
//! trips, tool-result image placement, recovery, usage and pricing through
//! the full layer stack — no credentials, no network beyond loopback.

mod live;
mod scripts;
mod server;

use std::sync::Arc;
use std::time::Duration;

use animist::agent::{AgentEvent, Cancellation};
use animist::provider::ProviderEvent;
use serde_json::{Value, json};

use crate::config::Config;
use crate::harness::{Conversation, ConversationBuilder, Event, EventSink, TurnOutcome};
use crate::provider::{ModelHandle, ProviderHandle};
use crate::tools::Workspace;

use scripts::Usage;
use server::{MockServer, Reply};

/// The wire dialect a kind speaks, which picks the script builders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// Anthropic Messages SSE (`/v1/messages`, `x-api-key`).
    Anthropic,
    /// OpenAI Chat Completions SSE (`/v1/chat/completions`).
    Chat,
    /// OpenAI Responses SSE (`/v1/responses`).
    Responses,
    /// Bedrock Converse AWS eventstream (`/model/{id}/converse-stream`).
    Converse,
}

impl Dialect {
    /// The request path the first conversation request hits.
    fn path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Converse => "/model/m/converse-stream",
        }
    }
}

/// One supported kind's wiring: the provider BML block (with `base_url`
/// pointed at the mock), its dialect, and the auth headers its requests
/// must carry.
struct Kind {
    /// The BML body of the provider block, `{}` replaced by the base URL.
    bml: &'static str,
    dialect: Dialect,
    auth: Auth,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// `x-api-key: test-key`.
    XApiKey,
    /// `authorization: Bearer test-key`.
    Bearer,
    /// No auth header at all.
    None,
    /// SigV4 (asserted by prefix only).
    SigV4,
}

/// The provider table: one row per supported config kind.
fn kinds() -> Vec<(&'static str, Kind)> {
    vec![
        (
            "anthropic",
            Kind {
                bml: "provider \"t\" { kind = \"anthropic\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Anthropic,
                auth: Auth::XApiKey,
            },
        ),
        (
            "openai (responses)",
            Kind {
                bml: "provider \"t\" { kind = \"openai\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Responses,
                auth: Auth::Bearer,
            },
        ),
        (
            "openai (chat_completions)",
            Kind {
                bml: "provider \"t\" { kind = \"openai\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  protocol = \"chat_completions\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "openai-compatible",
            Kind {
                bml: "provider \"t\" { kind = \"openai-compatible\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "synthetic",
            Kind {
                bml: "provider \"t\" { kind = \"synthetic\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "zai-coding-plan",
            Kind {
                bml: "provider \"t\" { kind = \"zai-coding-plan\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "ollama",
            Kind {
                bml: "provider \"t\" { kind = \"ollama\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::None,
            },
        ),
        (
            "llama-cpp",
            Kind {
                bml: "provider \"t\" { kind = \"llama-cpp\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "omlx",
            Kind {
                bml: "provider \"t\" { kind = \"omlx\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 100000 } }\n",
                dialect: Dialect::Chat,
                auth: Auth::Bearer,
            },
        ),
        (
            "amazon-bedrock (converse)",
            Kind {
                bml: "provider \"t\" { kind = \"amazon-bedrock\"\n  endpoint_url = \"http://127.0.0.1:{port}\"\n  region = \"us-east-1\"\n  discover_models = false\n  model \"m\" { context_length = 100000  protocol = \"converse\" } }\n",
                dialect: Dialect::Converse,
                auth: Auth::SigV4,
            },
        ),
        (
            "amazon-bedrock (anthropic_messages)",
            Kind {
                bml: "provider \"t\" { kind = \"amazon-bedrock\"\n  endpoint_url = \"http://127.0.0.1:{port}\"\n  region = \"us-east-1\"\n  discover_models = false\n  model \"m\" { context_length = 100000  protocol = \"anthropic_messages\" } }\n",
                dialect: Dialect::Anthropic,
                auth: Auth::SigV4,
            },
        ),
        (
            "amazon-bedrock (chat_completions)",
            Kind {
                bml: "provider \"t\" { kind = \"amazon-bedrock\"\n  endpoint_url = \"http://127.0.0.1:{port}\"\n  region = \"us-east-1\"\n  discover_models = false\n  model \"m\" { context_length = 100000  protocol = \"chat_completions\" } }\n",
                dialect: Dialect::Chat,
                auth: Auth::SigV4,
            },
        ),
    ]
}

/// The one-turn driver: build the provider from `bml` pointed at the mock
/// server, run one conversation turn over the scripted replies, and return
/// the server (for request assertions) with the events and outcome.
async fn converse(bml: &str, replies: Vec<Reply>) -> (MockServer, Vec<Event>, TurnOutcome) {
    converse_with(bml, replies, None, None).await
}

/// [`converse`] plus optional injected catalog and reauth hook.
async fn converse_with(
    bml: &str,
    replies: Vec<Reply>,
    catalog: Option<animist::catalog::Catalog>,
    reauth: Option<Arc<dyn crate::harness::ReauthHook>>,
) -> (MockServer, Vec<Event>, TurnOutcome) {
    converse_configured(
        bml,
        replies,
        catalog,
        reauth,
        crate::thinking::ThinkingConfig::Off,
    )
    .await
}

/// [`converse_with`] plus the request thinking, mirroring what an ACP
/// turn sends for a stored preference.
async fn converse_configured(
    bml: &str,
    replies: Vec<Reply>,
    catalog: Option<animist::catalog::Catalog>,
    reauth: Option<Arc<dyn crate::harness::ReauthHook>>,
    thinking: crate::thinking::ThinkingConfig,
) -> (MockServer, Vec<Event>, TurnOutcome) {
    let server = MockServer::spawn(replies).await;
    let config_text = format!(
        "compression {{ enabled = false }}\n{}",
        bml.replace("{port}", &server.port().to_string()),
    );
    let config = Config::parse(&config_text).expect("conformance config parses");
    let provider_config = config.providers.get("t").expect("provider alias t").clone();
    let credential = injected_key;
    let handle = ProviderHandle::from_config_with(&provider_config, catalog.as_ref(), &credential)
        .await
        .expect("provider constructs against the mock");
    let model: ModelHandle = handle
        .model(&provider_config, catalog.as_ref(), "m")
        .await
        .expect("model builds");

    let dir = tempfile::tempdir().expect("workspace dir");
    std::fs::write(dir.path().join("note.txt"), "the note contents\n").unwrap();
    std::fs::write(dir.path().join("img.png"), scripts::tiny_png()).unwrap();
    let workspace = Workspace::new(dir.path()).expect("workspace");
    let cwd = dir.path().display().to_string();
    let (sink, stream) = EventSink::new();
    let history = animist::history::History::default();
    let mut builder = ConversationBuilder::new(
        &config,
        &cwd,
        "",
        model,
        workspace.registry(&history).expect("registry"),
        sink,
    )
    .history(history)
    .workspace(workspace);
    if let Some(reauth) = reauth {
        builder = builder.reauth(reauth);
    }
    let conversation: Conversation = builder.build().expect("conversation builds");
    conversation.update_settings(|settings| settings.thinking = thinking);
    let mut stream = stream;

    // The driver emits the terminal Done just after the run resolves, so
    // await the run, then drain the sink through Done (bounded: a broken
    // run fails the assertions instead of hanging).
    let run = crate::harness::run_turn(&conversation, "hello there", &[], Cancellation::default());
    let outcome = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the turn completes");
    let mut events = Vec::new();
    // Bounded: a run that ends without its terminal Done fails the
    // assertions below instead of hanging the suite.
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        let done = matches!(
            &event,
            Event::Craft(crate::harness::CraftEvent::Done { .. })
        );
        events.push(event);
        if done {
            break;
        }
    }
    (server, events, outcome)
}

/// The turn-1 script body for a dialect: a plain text reply.
fn text_reply(dialect: Dialect, text: &str, usage: Usage) -> Vec<u8> {
    match dialect {
        Dialect::Anthropic => scripts::anthropic_text(text, usage),
        Dialect::Chat => scripts::chat_text(text, usage),
        Dialect::Responses => scripts::responses_text(text, usage),
        Dialect::Converse => scripts::converse_text(text, usage),
    }
}

/// The turn-1 script body for a dialect: one `read note.txt` tool call.
fn read_call(dialect: Dialect, usage: Usage) -> Vec<u8> {
    let input = json!({"path": "note.txt"});
    match dialect {
        Dialect::Anthropic => scripts::anthropic_tool_call("t1", "read", input, usage),
        Dialect::Chat => scripts::chat_tool_call("t1", "read", &input.to_string(), usage),
        Dialect::Responses => scripts::responses_tool_call("t1", "read", &input.to_string(), usage),
        Dialect::Converse => scripts::converse_tool_call("t1", "read", input, usage),
    }
}

fn sse_reply(body: Vec<u8>) -> Reply {
    Reply::sse(body)
}

/// The credential every keyed kind injects. A named fn (not a closure) so
/// the deliberate unboxed error type can carry its expectation.
#[expect(
    clippy::result_large_err,
    reason = "the crate error type is deliberately unboxed"
)]
fn injected_key(_env: &str) -> Result<String, crate::error::Error> {
    Ok("test-key".to_owned())
}

fn error_body(message: &str) -> Value {
    json!({"error": {"message": message, "type": "invalid_request_error"}})
}

// --- The per-kind table ---------------------------------------------------------------

/// Every supported kind round-trips a text turn through the full stack:
/// the request hits the dialect's path with the kind's auth, the reply
/// streams back as events, and the reported usage lands in the terminal
/// Done.
#[tokio::test]
async fn every_kind_round_trips_a_text_turn() {
    for (name, kind) in kinds() {
        let (server, events, outcome) = converse(
            kind.bml,
            vec![sse_reply(text_reply(
                kind.dialect,
                "the reply",
                Usage::new(120, 7),
            ))],
        )
        .await;

        let requests = server.recorded();
        assert_eq!(requests.len(), 1, "{name}: one request");
        let request = &requests[0];
        assert_eq!(request.method, "POST", "{name}");
        assert!(
            request.path.ends_with(kind.dialect.path()),
            "{name}: path {} does not end with {}",
            request.path,
            kind.dialect.path()
        );
        match kind.auth {
            Auth::XApiKey => assert_eq!(
                request.x_api_key.as_deref(),
                Some("test-key"),
                "{name}: x-api-key"
            ),
            Auth::Bearer => assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer test-key"),
                "{name}: bearer"
            ),
            Auth::None => {
                assert!(request.authorization.is_none(), "{name}: no auth header");
                assert!(request.x_api_key.is_none(), "{name}: no x-api-key");
            }
            Auth::SigV4 => assert!(
                request
                    .authorization
                    .as_deref()
                    .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256")),
                "{name}: SigV4 authorization"
            ),
        }
        // The request names the model and carries the system prompt.
        let body = &request.body;
        // Converse carries the model id in the URL path (asserted above).
        if kind.dialect != Dialect::Converse {
            assert_eq!(body["model"], "m", "{name}: model id");
        }
        let system = match kind.dialect {
            Dialect::Anthropic => body["system"].as_str().map(str::to_owned),
            Dialect::Chat => body["messages"]
                .as_array()
                .and_then(|messages| messages.first())
                .filter(|message| message["role"] == "system")
                .and_then(|message| message["content"].as_str())
                .map(str::to_owned),
            Dialect::Responses => body["instructions"].as_str().map(str::to_owned),
            Dialect::Converse => body["system"][0]["text"].as_str().map(str::to_owned),
        };
        assert!(
            system.is_some_and(|text| !text.is_empty()),
            "{name}: system prompt in {{}}"
        );

        let reply = match &outcome {
            TurnOutcome::Done { reply } => reply.clone(),
            other => panic!("{name}: expected Done, got {other:?}\nevents: {events:?}"),
        };
        assert_eq!(reply, "the reply", "{name}");
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Agent(AgentEvent::Provider {
                    event: ProviderEvent::TextDelta { text },
                    ..
                }) if text == "the reply"
            )),
            "{name}: the delta streamed through the layers"
        );
        let usage = events
            .iter()
            .find_map(|event| match event {
                Event::Craft(crate::harness::CraftEvent::Done { by_model, .. }) => {
                    Some(by_model.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: terminal Done: {events:?}"));
        let stored = usage
            .values()
            .next()
            .unwrap_or_else(|| panic!("{name}: one model billed"));
        assert_eq!(stored.input, 120, "{name}: input usage");
        assert_eq!(stored.output, 7, "{name}: output usage");
    }
}

/// Every supported kind round-trips a tool call: the model calls `read`,
/// the real tool executes against the workspace, and the follow-up request
/// carries the result in the dialect's shape.
#[tokio::test]
async fn every_kind_round_trips_a_tool_call() {
    for (name, kind) in kinds() {
        let (server, _events, outcome) = converse(
            kind.bml,
            vec![
                sse_reply(read_call(kind.dialect, Usage::new(50, 3))),
                sse_reply(text_reply(kind.dialect, "done", Usage::new(80, 2))),
            ],
        )
        .await;
        assert!(
            matches!(outcome, TurnOutcome::Done { .. }),
            "{name}: {outcome:?}"
        );
        let requests = server.recorded();
        assert_eq!(requests.len(), 2, "{name}: call then result");
        let follow_up = &requests[1].body;
        let carries_result = match kind.dialect {
            Dialect::Anthropic => follow_up["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block["type"] == "tool_result"
                                && block["tool_use_id"] == "t1"
                                && block["content"][0]["text"]
                                    .as_str()
                                    .is_some_and(|text| text.contains("the note contents"))
                        })
                    })
                })
            }),
            Dialect::Chat => follow_up["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == "t1"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("the note contents"))
                })
            }),
            Dialect::Responses => follow_up["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "function_call_output"
                        && item["call_id"] == "t1"
                        && item["output"]
                            .as_str()
                            .is_some_and(|text| text.contains("the note contents"))
                })
            }),
            Dialect::Converse => follow_up["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block["toolResult"]["toolUseId"] == "t1"
                                && block["toolResult"]["content"][0]["text"]
                                    .as_str()
                                    .is_some_and(|text| text.contains("the note contents"))
                        })
                    })
                })
            }),
        };
        assert!(carries_result, "{name}: follow-up carries the result");
    }
}

// --- Thinking -------------------------------------------------------------------------

fn thinking_reply(dialect: Dialect, usage: Usage) -> Vec<u8> {
    match dialect {
        Dialect::Anthropic => scripts::anthropic_thinking("let me think", "the answer", usage),
        Dialect::Chat => scripts::chat_thinking("let me think", "the answer", usage),
        Dialect::Responses => scripts::responses_thinking("let me think", "the answer", usage),
        Dialect::Converse => {
            // Converse rejects non-off thinking configs; the reasoning path
            // is exercised on the other dialects.
            scripts::converse_text("the answer", usage)
        }
    }
}

/// Thinking streams surface as reasoning deltas on every dialect that
/// carries them (anthropic, chat-completions kinds, responses).
#[tokio::test]
async fn thinking_surfaces_as_reasoning_deltas() {
    for (name, kind) in kinds() {
        if kind.dialect == Dialect::Converse {
            continue;
        }
        let (_server, events, outcome) = converse(
            kind.bml,
            vec![sse_reply(thinking_reply(kind.dialect, Usage::new(10, 5)))],
        )
        .await;
        assert!(
            matches!(outcome, TurnOutcome::Done { .. }),
            "{name}: {outcome:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Agent(AgentEvent::Provider {
                    event: ProviderEvent::ThinkingDelta { text },
                    ..
                }) if text == "let me think"
            )),
            "{name}: the thinking delta streamed through"
        );
    }
}

/// A models.dev-style catalog with one reasoning model `m` reporting the
/// given effort values, mirroring what the provider family's real entries
/// carry (hosted OpenAI never lists `max`; converse models carry no
/// reasoning metadata at all).
fn reasoning_catalog(key: &str, efforts: &[&str]) -> animist::catalog::Catalog {
    let doc = json!({ key: {
        "id": key,
        "name": key,
        "models": {"m": {
            "reasoning": !efforts.is_empty(),
            "reasoning_options": [{"type": "effort", "values": efforts}],
            "limit": {"context": 100_000, "output": 8_192}
        }}
    }});
    animist::catalog::Catalog::from_slice(&serde_json::to_vec(&doc).expect("catalog serializes"))
        .expect("catalog parses")
}

/// Reasoning effort values models.dev reports for the kind's provider
/// family; an empty slice marks a family whose models carry no thinking
/// metadata (only `off` is offered).
fn family_efforts(kind: &Kind) -> Vec<&'static str> {
    match kind.dialect {
        Dialect::Anthropic => vec!["low", "medium", "high", "max"],
        Dialect::Responses | Dialect::Chat => vec!["minimal", "low", "medium", "high"],
        Dialect::Converse => vec![],
    }
}

/// Whether a recorded request's reasoning fields are valid for the
/// dialect: the thinking knob, when present, must be a shape the wire
/// accepts, never a value the dialect rejects.
fn reasoning_wire_is_valid(dialect: Dialect, body: &Value) -> bool {
    match dialect {
        Dialect::Anthropic => {
            let valid_effort = body["output_config"]["effort"]
                .as_str()
                .is_none_or(|effort| ["low", "medium", "high", "xhigh", "max"].contains(&effort));
            let valid_thinking = match &body["thinking"] {
                Value::Null => true,
                thinking => {
                    thinking["type"].is_string()
                        && ["disabled", "adaptive", "enabled"]
                            .contains(&thinking["type"].as_str().unwrap_or_default())
                }
            };
            valid_effort && valid_thinking
        }
        Dialect::Chat | Dialect::Responses => {
            body["reasoning_effort"].as_str().is_none_or(|effort| {
                ["none", "minimal", "low", "medium", "high", "xhigh"].contains(&effort)
            })
        }
        // Converse rejects every non-off thinking config, so a valid
        // request carries no reasoning fields at all.
        Dialect::Converse => {
            body.get("reasoning_config").is_none()
                && body["additionalModelRequestFields"]["thinking"].is_null()
        }
    }
}

/// Every offered thinking choice, driven through the same
/// preference-to-effective path an ACP turn applies, completes the turn
/// and lands dialect-valid reasoning fields on the request: no offered
/// level can fail the turn with an "agent turn failed" error.
#[tokio::test]
async fn offered_thinking_choices_complete_turns_with_valid_wire_fields() {
    for (name, kind) in kinds() {
        let key = crate::provider::catalog_key(&kind_config(&kind));
        let efforts = family_efforts(&kind);
        let catalog = reasoning_catalog(key, &efforts);
        let info = catalog.model(key, "m").expect("catalog model").info.clone();
        let offered =
            crate::thinking::choices_for(crate::thinking::ThinkingConfig::Off, Some(&info));
        assert!(
            !offered.is_empty(),
            "{name}: the option always offers something"
        );
        // Plus the stale-preference case that motivated the fix: values a
        // different model offered (or an explicit budget) must still snap
        // to something the dialect accepts.
        let mut driven = offered;
        driven.extend([
            crate::thinking::ThinkingConfig::Effort(crate::thinking::Effort::Max),
            crate::thinking::ThinkingConfig::Effort(crate::thinking::Effort::Minimal),
            crate::thinking::ThinkingConfig::Budget(4096),
        ]);
        for choice in driven {
            let effective = crate::thinking::effective_for(choice, Some(&info));
            let (server, _events, outcome) = converse_configured(
                kind.bml,
                vec![sse_reply(thinking_reply(kind.dialect, Usage::new(10, 5)))],
                Some(catalog.clone()),
                None,
                effective,
            )
            .await;
            assert!(
                matches!(outcome, TurnOutcome::Done { .. }),
                "{name}: {choice:?} (effective {effective:?}): {outcome:?}"
            );
            let requests = server.recorded();
            assert_eq!(requests.len(), 1, "{name}: one request");
            assert!(
                reasoning_wire_is_valid(kind.dialect, &requests[0].body),
                "{name}: {choice:?} (effective {effective:?}) produced invalid reasoning fields: {}",
                requests[0].body
            );
        }
    }
}

/// A manual `supports_thinking` override must not be lost by the
/// turn-time snap: the resolved capability (override applied) governs, so
/// an adaptive preference still reaches the wire even though no catalog
/// metadata reports the model as reasoning-capable.
#[tokio::test]
async fn manual_thinking_overrides_survive_the_turn_time_snap() {
    for (name, kind) in kinds() {
        if kind.dialect == Dialect::Converse {
            // Converse rejects every non-off thinking config regardless of
            // overrides; the manual override is not expressible there.
            continue;
        }
        // The kind BMLs embed the manual model block in two shapes (with
        // and without a trailing protocol field); patch the field itself.
        let bml = kind.bml.replace(
            "context_length = 100000",
            "context_length = 100000 supports_thinking = true",
        );
        let effective = crate::thinking::effective_for(
            crate::thinking::ThinkingConfig::Adaptive,
            Some(&crate::testing::model_info(
                "m", 100_000, 8_192, true, false,
            )),
        );
        let (server, events, outcome) = converse_configured(
            &bml,
            vec![sse_reply(thinking_reply(kind.dialect, Usage::new(10, 5)))],
            None,
            None,
            effective,
        )
        .await;
        assert!(
            matches!(outcome, TurnOutcome::Done { .. }),
            "{name}: {outcome:?}"
        );
        let requests = server.recorded();
        assert_eq!(requests.len(), 1, "{name}: one request");
        assert!(
            reasoning_wire_is_valid(kind.dialect, &requests[0].body),
            "{name}: invalid reasoning fields: {}",
            requests[0].body
        );
        // And thinking actually streamed, proving it was on.
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Agent(AgentEvent::Provider {
                    event: ProviderEvent::ThinkingDelta { .. },
                    ..
                })
            )),
            "{name}: the thinking delta streamed through"
        );
    }
}

/// A kind's provider config, for the models.dev catalog key.
fn kind_config(kind: &Kind) -> crate::config::ProviderConfig {
    let config = Config::parse(&format!(
        "compression {{ enabled = false }}\n{}",
        kind.bml.replace("{port}", "0"),
    ))
    .expect("kind config parses");
    config.providers.get("t").expect("provider alias t").clone()
}

// --- Tool-result images ---------------------------------------------------------------

fn view_image_call(dialect: Dialect, usage: Usage) -> Vec<u8> {
    let input = json!({"path": "img.png"});
    match dialect {
        Dialect::Anthropic => scripts::anthropic_tool_call("t1", "view_image", input, usage),
        Dialect::Chat => scripts::chat_tool_call("t1", "view_image", &input.to_string(), usage),
        Dialect::Responses => {
            scripts::responses_tool_call("t1", "view_image", &input.to_string(), usage)
        }
        Dialect::Converse => scripts::converse_tool_call("t1", "view_image", input, usage),
    }
}

/// The follow-up request's JSON text mentions the image for the dialects
/// that keep tool-result images inline.
#[tokio::test]
async fn tool_result_images_stay_inline_where_accepted() {
    for (name, kind) in kinds() {
        if kind.dialect == Dialect::Chat {
            continue; // relocation suite below
        }
        let (server, _events, outcome) = converse(
            kind.bml,
            vec![
                sse_reply(view_image_call(kind.dialect, Usage::new(30, 2))),
                sse_reply(text_reply(kind.dialect, "seen", Usage::new(40, 1))),
            ],
        )
        .await;
        assert!(
            matches!(outcome, TurnOutcome::Done { .. }),
            "{name}: {outcome:?}"
        );
        let follow_up = server.recorded()[1].text();
        assert!(
            follow_up.contains("image"),
            "{name}: the result block mentions the image"
        );
        let encoded = kind.dialect == Dialect::Anthropic || kind.dialect == Dialect::Responses;
        if encoded {
            assert!(
                follow_up.contains("png") || follow_up.contains("base64"),
                "{name}: the image payload rides the result"
            );
        }
    }
}

/// Chat Completions rejects tool-result images, so the request layer
/// relocates them: the tool message keeps text only and a following user
/// message carries the image.
#[tokio::test]
async fn chat_completions_relocates_tool_result_images() {
    let kind = kinds()
        .into_iter()
        .find(|(name, _)| *name == "openai-compatible")
        .expect("openai-compatible row")
        .1;
    let (server, _events, outcome) = converse(
        kind.bml,
        vec![
            sse_reply(view_image_call(kind.dialect, Usage::new(30, 2))),
            sse_reply(text_reply(kind.dialect, "seen", Usage::new(40, 1))),
        ],
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let messages = server.recorded()[1].body["messages"]
        .as_array()
        .expect("messages")
        .clone();
    let tool_index = messages
        .iter()
        .position(|message| message["role"] == "tool")
        .expect("the tool result message");
    let tool_message = &messages[tool_index];
    let tool_text = match &tool_message["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    assert!(
        !tool_text.contains("image_url"),
        "the tool result carries no image"
    );
    let relocated = messages[tool_index..].iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image_url"))
    });
    if !relocated {
        panic!(
            "no relocated image; messages: {}",
            serde_json::to_string_pretty(&messages).unwrap_or_default()
        );
    }
}

// --- Recovery -------------------------------------------------------------------------

/// A 429 then success: the recovery layer retries with backoff and the run
/// completes; the server saw both attempts.
#[tokio::test]
async fn rate_limit_errors_retry_to_success() {
    let (server, events, outcome) = converse(
        kinds()[0].1.bml,
        vec![
            Reply::json(429, error_body("rate limited")),
            sse_reply(text_reply(Dialect::Anthropic, "ok", Usage::new(1, 1))),
        ],
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        server.request_count(),
        2,
        "both attempts served; events: {events:?}; pending {}",
        server.pending()
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Retry { .. })
        )),
        "the retry surfaced as a craft event: {events:?}"
    );
}

/// A 5xx then success retries the same way.
#[tokio::test]
async fn server_errors_retry_to_success() {
    let (server, _events, outcome) = converse(
        kinds()[0].1.bml,
        vec![
            Reply::json(503, error_body("overloaded")),
            sse_reply(text_reply(Dialect::Anthropic, "ok", Usage::new(1, 1))),
        ],
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(server.request_count(), 2);
}

/// A 401 parks the run on the reauth hook; a refresh that keeps the model
/// resumes and completes.
struct RefreshingReauth;
#[async_trait::async_trait]
impl crate::harness::ReauthHook for RefreshingReauth {
    async fn reauth(&self, _attempt: u32) -> std::result::Result<Option<ModelHandle>, String> {
        Ok(None) // credential refreshed in place; keep the model
    }
}

#[tokio::test]
async fn auth_failures_wait_for_reauth_then_resume() {
    let (server, events, outcome) = converse_with(
        kinds()[0].1.bml,
        vec![
            Reply::json(401, error_body("invalid api key")),
            sse_reply(text_reply(Dialect::Anthropic, "ok", Usage::new(1, 1))),
        ],
        None,
        Some(Arc::new(RefreshingReauth)),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(server.request_count(), 2);
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::AuthRequired { .. })
        )),
        "the auth wait surfaced: {events:?}"
    );
}

/// A context-length overflow body from the wire classifies as overflow,
/// not as a transient error: with nothing left to compact the run fails
/// fast carrying the provider's message, and no backoff retries fire (a
/// misclassification would burn transient retries against the mock's
/// fallback 500s — the ladder itself is pinned in `harness::tests::overflow`).
#[tokio::test]
async fn overflow_bodies_classify_as_overflow_not_transient() {
    let bml = "compaction \"vcc\" { context = \"0.6\" }\nprovider \"t\" { kind = \"anthropic\"\n  base_url = \"http://127.0.0.1:{port}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" { context_length = 900 } }\n";
    let overflow = Reply::json(
        400,
        json!({"error": {"message": "This model's maximum context length is 900 tokens. However, your messages resulted in 9000 tokens.", "type": "invalid_request_error", "code": "context_length_exceeded"}}),
    );
    let (server, events, outcome) = converse(
        bml,
        vec![
            sse_reply(scripts::anthropic_tool_call(
                "t1",
                "read",
                json!({"path": "note.txt"}),
                Usage::new(50, 3),
            )),
            overflow,
        ],
    )
    .await;
    let TurnOutcome::Failed(message) = &outcome else {
        panic!("unshrinkable overflow fails the run: {outcome:?}");
    };
    assert!(
        message.contains("maximum context length"),
        "the provider's overflow message survives: {message}"
    );
    assert_eq!(
        server.request_count(),
        2,
        "no transient retries after the overflow (events: {events:?})"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Retry { .. })
        )),
        "overflow recovery compacts instead of backing off: {events:?}"
    );
}

// --- Usage and pricing ------------------------------------------------------------------

/// Bedrock retries 429/503 inside the provider (its own attempt budget)
/// before surfacing anything: the run succeeds, both attempts are served,
/// and craft's recovery layer never fires.
#[tokio::test]
async fn bedrock_in_provider_retries_do_not_stack_with_craft_recovery() {
    let kind = kinds()
        .into_iter()
        .find(|(name, _)| *name == "amazon-bedrock (converse)")
        .expect("converse row")
        .1;
    let retry = Reply {
        status: 429,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("retry-after".into(), "0".into()),
        ],
        body: br#"{"message":"Too many requests"}"#.to_vec(),
    };
    let (server, events, outcome) = converse(
        kind.bml,
        vec![
            retry,
            sse_reply(text_reply(Dialect::Converse, "ok", Usage::new(1, 1))),
        ],
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(server.request_count(), 2, "the provider retried once");
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Retry { .. })
        )),
        "craft recovery stayed out of it: {events:?}"
    );
}

/// A models.dev-style catalog feeds the model handle's pricing (USD per
/// million), and the wire-reported usage reaches the terminal Done with
/// exact counters. Cost *settlement* additionally reads craft's on-disk
/// models.dev cache (process-wide path resolution, no offline seam) and is
/// pinned at the `usage::settle_session` unit level instead.
#[tokio::test]
async fn catalog_pricing_reaches_the_model_and_usage_reaches_the_done() {
    let catalog_json = json!({
        "anthropic": {
            "id": "anthropic",
            "name": "Anthropic",
            "models": {"m": {
                "id": "m",
                "name": "M",
                "cost": {"input": 3.0, "output": 15.0},
                "limit": {"context": 100000, "output": 8192}
            }}
        }
    });
    let catalog = animist::catalog::Catalog::from_slice(
        &serde_json::to_vec(&catalog_json).expect("catalog serializes"),
    )
    .expect("catalog parses");
    let (server, events, outcome) = converse_with(
        kinds()[0].1.bml,
        vec![sse_reply(text_reply(
            Dialect::Anthropic,
            "priced",
            Usage::new(1_000, 2_000),
        ))],
        Some(catalog),
        None,
    )
    .await;
    // The priced model rode the requests the mock served.
    assert_eq!(server.request_count(), 1);
    let usage = events
        .iter()
        .find_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Done { by_model, .. }) => {
                Some(by_model.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("terminal Done: {events:?}"));
    let stored = usage.values().next().expect("one model billed");
    assert_eq!(stored.input, 1_000);
    assert_eq!(stored.output, 2_000);
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
}

// --- The headless surface end to end ---------------------------------------------------

/// The print path persists its turns: a `--print` run through the real
/// headless entry point leaves entry records in the session file, not just
/// the fresh header the store writes on open (12's perf harness caught the
/// unwired persister; this is the regression pin).
#[tokio::test]
async fn the_print_path_persists_its_entries() {
    let server = MockServer::spawn(vec![sse_reply(text_reply(
        Dialect::Chat,
        "headless reply",
        Usage::new(10, 4),
    ))])
    .await;
    let config = Config::parse(&format!(
        "compression {{ enabled = false }}\nprovider \"t\" {{\n  kind = \"openai-compatible\"\n  base_url = \"http://127.0.0.1:{}/v1\"\n  api_key_env = \"CONF_PRINT_PATH_KEY\"\n  discover_models = false\n  model \"m\" {{ context_length = 100000 }}\n}}\n",
        server.port(),
    ))
    .expect("config parses");
    let state = tempfile::tempdir().expect("state dir");
    // The headless path reads the key from the environment like a real
    // run. SAFETY: tests run multi-threaded, but no other test reads or
    // writes this variable (the conformance table injects credentials
    // instead), so the race window is unobservable.
    unsafe { std::env::set_var("CONF_PRINT_PATH_KEY", "test-key") };
    let query = crate::cli::HeadlessQuery {
        prompt: "hello".into(),
        context: Vec::new(),
        images: Vec::new(),
        model: Some("t/m".into()),
        output_format: crate::cli::OutputFormat::Text,
        verbose: false,
        mode: crate::cli::CliMode::default(),
        session_id: None,
        policy: crate::cli::PermissionPolicy::Yolo,
        tool_policy: Vec::new(),
    };
    crate::cli::run_headless_query_with(
        config,
        query,
        Some(crate::storage::StateDir::from_path(
            state.path().to_path_buf(),
        )),
    )
    .await
    .expect("headless run completes");

    let sessions = state.path().join("sessions");
    let persisted = std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .expect("one session file");
    let body = std::fs::read_to_string(&persisted).unwrap();
    assert!(
        body.contains("\"t\":\"entry\""),
        "entry records persisted: {body}"
    );
    assert_eq!(server.request_count(), 1);
}

/// The auto-review LLM path (decision endpoint disabled) round-trips
/// through the Bedrock provider: one no-tools request on the model's own
/// protocol, the reviewer system prompt, and a scripted verdict parsed
/// into a decision.
#[tokio::test]
async fn bedrock_serves_the_auto_review_llm_path() {
    for name in [
        "amazon-bedrock (converse)",
        "amazon-bedrock (anthropic_messages)",
        "amazon-bedrock (chat_completions)",
    ] {
        let kind = kinds()
            .into_iter()
            .find(|(kind_name, _)| *kind_name == name)
            .expect("bedrock kind")
            .1;
        let verdict = "{\"verdict\":\"allow\",\"risk\":\"low\",\"rationale\":\"fine\"}";
        let reply = match kind.dialect {
            Dialect::Converse => scripts::converse_text(verdict, Usage::new(10, 5)),
            Dialect::Anthropic => scripts::anthropic_text(verdict, Usage::new(10, 5)),
            Dialect::Chat | Dialect::Responses => scripts::chat_text(verdict, Usage::new(10, 5)),
        };
        let server = MockServer::spawn(vec![sse_reply(reply)]).await;
        let config_text = format!(
            "compression {{ enabled = false }}\n{}",
            kind.bml.replace("{port}", &server.port().to_string())
        );
        let config = Config::parse(&config_text).expect("config parses");
        let provider_config = config.providers.get("t").expect("provider t").clone();
        let handle = ProviderHandle::from_config_with(&provider_config, None, &injected_key)
            .await
            .expect("provider constructs against the mock");
        let model = handle
            .model(&provider_config, None, "m")
            .await
            .expect("model builds");
        let decision = crate::auto_review::review(
            &model,
            "write",
            &["/tmp/x".into()],
            config.agent.prompt_format,
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: reviewer call failed: {error}"));
        assert_eq!(
            decision.verdict,
            crate::auto_review::Verdict::Allow,
            "{name}"
        );
        assert_eq!(decision.risk, crate::auto_review::Risk::Low, "{name}");
        let requests = server.recorded();
        assert_eq!(requests.len(), 1, "{name}: one reviewer request");
        assert!(
            requests[0].path.ends_with(kind.dialect.path()),
            "{name}: path"
        );
        assert!(
            requests[0]
                .authorization
                .as_deref()
                .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256")),
            "{name}: the reviewer request is signed"
        );
        let system = match kind.dialect {
            Dialect::Converse => requests[0].body["system"][0]["text"].as_str(),
            _ => requests[0].body["system"].as_str().or_else(|| {
                requests[0].body["messages"]
                    .as_array()
                    .and_then(|messages| messages.first())
                    .filter(|message| message["role"] == "system")
                    .and_then(|message| message["content"].as_str())
            }),
        };
        assert!(
            system.is_some_and(|text| text.contains("permission reviewer")),
            "{name}: the reviewer system prompt rides the request"
        );
    }
}
