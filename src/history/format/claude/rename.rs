use super::session_id_of;
use crate::error::{AppError, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Serialize)]
struct CustomTitleRecord<'a> {
    #[serde(rename = "type")]
    record_type: &'static str,
    #[serde(rename = "customTitle")]
    custom_title: &'a str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
}

#[derive(Serialize)]
struct AgentNameRecord<'a> {
    #[serde(rename = "type")]
    record_type: &'static str,
    #[serde(rename = "agentName")]
    agent_name: &'a str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
}

pub fn append_session_rename(path: &Path, title: &str) -> Result<()> {
    let session_id = session_id_of(path).ok_or_else(|| {
        AppError::ConfigError(format!("Invalid session path: {}", path.display()))
    })?;

    let custom_title = CustomTitleRecord {
        record_type: "custom-title",
        custom_title: title,
        session_id,
    };
    let agent_name = AgentNameRecord {
        record_type: "agent-name",
        agent_name: title,
        session_id,
    };

    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    if len > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }

    serde_json::to_writer(&mut file, &custom_title)?;
    file.write_all(b"\n")?;
    serde_json::to_writer(&mut file, &agent_name)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Read;

    fn lines(path: &Path) -> Vec<Value> {
        let mut text = String::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn append_session_rename_writes_jsonl_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc123.jsonl");
        std::fs::write(&path, "{}").unwrap();

        append_session_rename(&path, "quoted \"title\"\nnext").unwrap();

        let values = lines(&path);
        assert_eq!(values.len(), 3);
        assert_eq!(values[1]["type"], "custom-title");
        assert_eq!(values[1]["customTitle"], "quoted \"title\"\nnext");
        assert_eq!(values[1]["sessionId"], "abc123");
        assert_eq!(values[2]["type"], "agent-name");
        assert_eq!(values[2]["agentName"], "quoted \"title\"\nnext");
        assert_eq!(values[2]["sessionId"], "abc123");
    }

    #[test]
    fn append_session_rename_allows_empty_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc123.jsonl");
        std::fs::write(&path, "{}\n").unwrap();

        append_session_rename(&path, "").unwrap();

        let values = lines(&path);
        assert_eq!(values[1]["customTitle"], "");
        assert_eq!(values[2]["agentName"], "");
    }
}
