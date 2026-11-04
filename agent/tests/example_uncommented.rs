//! agent.example.bml must still parse with every commented snippet
//! uncommented, including the mcp blocks.

use std::collections::HashMap;

#[test]
fn parses_example_uncommented() {
    // `${VAR}` references are never expanded here, so no env setup needed.
    let raw = include_str!("../../agent/agent.example.bml");
    let mut out = String::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let uncommented = trimmed
            .strip_prefix("# ")
            .or_else(|| trimmed.strip_prefix('#'))
            .map(str::trim_start);
        let is_stmt = uncommented.is_some_and(|s| {
            s == "}" || s.contains(" = ") || s.ends_with('{')
        });
        if is_stmt {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str(uncommented.unwrap());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    craft::config::Config::parse(&out).unwrap();

    let doc = craft::bml::parse(&out).unwrap();
    let mut servers: HashMap<String, craft::mcp::config::RawServerConfig> = HashMap::new();
    for (id, labels, block) in doc.blocks() {
        if id != "mcp" {
            continue;
        }
        let name = labels.first().and_then(|l| l.as_string().cloned()).unwrap();
        let raw: craft::mcp::config::RawServerConfig =
            serde_json::from_value(craft::bml::container_json(block))
                .unwrap_or_else(|e| panic!("mcp server {name:?}: {e}"));
        servers.insert(name, raw);
    }
    assert_eq!(servers.len(), 3);
}
