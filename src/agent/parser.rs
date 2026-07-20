use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactRecord {
    pub tag: String,
    pub fields: Vec<(String, String)>,
    pub text: Option<String>,
}

pub fn parse_compact(input: &str) -> std::result::Result<Vec<CompactRecord>, String> {
    if input.is_empty() || !input.ends_with('\n') {
        return Err("compact output must end with a newline".to_string());
    }
    input.lines().map(parse_compact_line).collect()
}

fn parse_compact_line(line: &str) -> std::result::Result<CompactRecord, String> {
    if let Some(body) = line.strip_prefix('|') {
        return Ok(CompactRecord {
            tag: "body".to_string(),
            fields: Vec::new(),
            text: Some(body.strip_prefix(' ').unwrap_or(body).to_string()),
        });
    }
    let (atoms, text) = line
        .split_once(" | ")
        .map_or((line, None), |(atoms, text)| {
            (atoms, Some(text.to_string()))
        });
    let mut atoms = atoms.split_whitespace();
    let first = atoms
        .next()
        .ok_or_else(|| "empty compact record".to_string())?;
    let (tag, mut fields) = if first == "protocol" {
        let family = atoms
            .next()
            .ok_or_else(|| "protocol record requires a family".to_string())?;
        (
            "protocol".to_string(),
            vec![("family".to_string(), family.to_string())],
        )
    } else if first.starts_with('m') && first[1..].chars().all(|ch| ch.is_ascii_digit()) {
        (
            "outline".to_string(),
            vec![("message".to_string(), first.to_string())],
        )
    } else {
        (first.to_string(), Vec::new())
    };
    let allowed = [
        "protocol",
        "budget",
        "policy",
        "identity",
        "diagnostics",
        "continuation",
        "command",
        "grammar",
        "query",
        "conversation",
        "groups",
        "title",
        "hit",
        "read",
        "message",
        "outline",
        "seg",
        "continue",
    ];
    if !allowed.contains(&tag.as_str()) {
        return Err(format!("unknown compact record type: {tag}"));
    }
    if matches!(tag.as_str(), "message" | "seg" | "continue") {
        let value = atoms
            .next()
            .ok_or_else(|| format!("{tag} record requires a positional atom"))?;
        fields.push((
            match tag.as_str() {
                "message" => "message",
                "seg" => "range",
                _ => "action",
            }
            .to_string(),
            value.to_string(),
        ));
    }
    for atom in atoms {
        let (key, value) = atom
            .split_once('=')
            .ok_or_else(|| format!("compact atom lacks '=': {atom}"))?;
        if key.is_empty() || value.is_empty() {
            return Err(format!("malformed compact atom: {atom}"));
        }
        fields.push((key.to_string(), unescape_atom(value)?));
    }
    if tag == "protocol" && !fields.iter().any(|(key, _)| key == "v") {
        return Err("protocol record requires v=".to_string());
    }
    Ok(CompactRecord { tag, fields, text })
}

pub fn unescape_atom(value: &str) -> std::result::Result<String, String> {
    let mut bytes = Vec::with_capacity(value.len());
    let input = value.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err("incomplete percent escape".to_string());
            }
            let encoded = std::str::from_utf8(&input[index + 1..index + 3])
                .map_err(|_| "invalid percent escape".to_string())?;
            bytes.push(
                u8::from_str_radix(encoded, 16)
                    .map_err(|_| "invalid percent escape".to_string())?,
            );
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| "atom is not valid UTF-8".to_string())
}

pub fn parse_jsonl(input: &str) -> std::result::Result<Vec<Value>, String> {
    if input.is_empty() || !input.ends_with('\n') {
        return Err("JSONL output must end with a newline".to_string());
    }
    input
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let value: Value = serde_json::from_str(line)
                .map_err(|error| format!("invalid JSONL line {}: {error}", index + 1))?;
            let record_type = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("JSONL line {} lacks a string type", index + 1))?;
            let allowed = [
                "capabilities",
                "header",
                "conversation",
                "group",
                "hit",
                "message",
                "outline",
                "continuation",
                "warning",
                "error",
            ];
            if !allowed.contains(&record_type) {
                return Err(format!("unknown JSONL record type: {record_type}"));
            }
            if value.get("schema").and_then(Value::as_u64).is_none() {
                return Err(format!("JSONL line {} lacks schema", index + 1));
            }
            Ok(value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::protocol::escape_atom;

    #[test]
    fn atom_escape_round_trips_utf8_and_delimiters() {
        let original = "界 space%=|\n";
        assert_eq!(unescape_atom(&escape_atom(original)).unwrap(), original);
    }

    #[test]
    fn parser_accepts_protocol_record_grammar() {
        let compact = concat!(
            "protocol agent-search v=5 mode=lexical cut=none chars=none policy=per-hit hits=1\n",
            "query text=needle hits=1\n",
            "conversation project=pr_a uuid=none ref=ch_a revision=rv_a\n",
            "groups count=1\n",
            "title project=pr_a uuid=none ref=ch_a revision=rv_a | title text\n",
            "hit project=pr_a uuid=none ref=ch_a revision=rv_a anchors=ma_a source=lexical score=1.0 focus=m1..m1 | evidence\n",
            "read ref=ch_a:m1..m2 focus=m1..m1 revision=rv_a tools=false tool-results=false thinking=false subagents=false\n",
            "message m1 role=user line=1 anchor=ma_a\n",
            "| body\n",
            "m1 role=user chars=4 anchor=ma_a | body\n",
            "seg m1..m10 chars=40 anchors=ma_a..ma_b | first / last\n",
            "continue read refs=ch_a:m2 revision=rv_a\n",
            "protocol agent-warning v=1 kind=skipped detail=detail\n",
        );
        let parsed = parse_compact(compact).unwrap();
        assert_eq!(parsed.len(), 13);
    }

    #[test]
    fn parser_rejects_malformed_compact_records() {
        assert!(parse_compact("protocol agent-search mode=lexical\n").is_err());
        assert!(parse_compact("unknown key=value\n").is_err());
        assert!(parse_compact("query malformed\n").is_err());
    }

    #[test]
    fn jsonl_parser_accepts_every_tagged_record_type() {
        let input = [
            "capabilities",
            "header",
            "conversation",
            "group",
            "hit",
            "message",
            "outline",
            "continuation",
            "warning",
            "error",
        ]
        .into_iter()
        .map(|record_type| serde_json::json!({"type":record_type,"schema":1}).to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        assert_eq!(parse_jsonl(&input).unwrap().len(), 10);
    }

    #[test]
    fn parser_rejects_partial_or_untagged_jsonl() {
        assert!(parse_jsonl("{\"type\":\"hit\"}").is_err());
        assert!(parse_jsonl("{\"schema\":1}\n").is_err());
    }
}
