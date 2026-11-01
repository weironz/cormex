//! Read a bounded set of archived execution outputs, never execute them.
use cormex_memory::dialogue::ToolEvidence;
use cormex_store::Store;

pub async fn collect(
    store: &Store,
    session: &str,
    version: &str,
    from: i64,
    through: i64,
) -> anyhow::Result<Vec<ToolEvidence>> {
    let rows:Vec<(String,i64,String,String,String,String,i32)>=sqlx::query_as(
 "SELECT r.id::text,r.seq,coalesce(c.tool_name,''),coalesce(left(c.content::text,1200),''),left(r.text,2400),right(r.text,1600),length(r.text)
 FROM transcript_events r LEFT JOIN LATERAL(SELECT tool_name,content FROM transcript_events c
 WHERE c.session_id=r.session_id AND c.parser_version=r.parser_version AND c.kind='tool_call' AND c.tool_call_id=r.tool_call_id AND c.seq<r.seq ORDER BY c.seq DESC LIMIT 1)c ON true
 WHERE r.session_id=$1 AND r.parser_version=$2 AND r.seq>=$3 AND r.seq<=$4 AND r.kind='tool_result' AND r.text IS NOT NULL ORDER BY r.seq DESC LIMIT 16")
 .bind(session).bind(version).bind(from).bind(through).fetch_all(store.pool()).await?;
    let mut candidates = Vec::new();
    for (id, seq, tool_name, command, head, tail, len) in rows {
        let text = if len > 4000 {
            format!("{head}\n[输出省略]\n{tail}")
        } else if len > 2400 {
            // The overlap preserves exact source quotes without repeating the middle.
            let skip = (4000 - len) as usize;
            format!("{head}{}", tail.chars().skip(skip).collect::<String>())
        } else {
            head
        };
        if cormex_memory::redaction::scan(&format!("{command}\n{text}")).is_some() {
            continue;
        }
        let trusted = matches!(tool_name.as_str(), "exec_command" | "write_stdin")
            || (tool_name.ends_with("exec")
                && (command.contains("exec_command") || command.contains("write_stdin")));
        let exit = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("Process exited with code "))
            .and_then(|s| s.trim().parse::<i32>().ok());
        let failed = exit.is_some_and(|n| n != 0) || text.contains("test result: FAILED");
        let successful = trusted
            && !failed
            && (exit == Some(0)
                || (!text.contains("Process running with session ID")
                    && text.contains("test result: ok.")));
        let priority = usize::from(text.contains("test result: ok.")) * 2 + usize::from(successful);
        candidates.push((
            priority,
            seq,
            ToolEvidence {
                id,
                seq,
                tool_name,
                command,
                text,
                successful,
            },
        ));
    }
    candidates.sort_by_key(|c| std::cmp::Reverse((c.0, c.1)));
    let mut result = Vec::new();
    let mut budget = 8000;
    for (_, _, record) in candidates {
        let size = record.text.chars().count() + record.command.chars().count();
        if size > budget {
            continue;
        }
        budget -= size;
        result.push(record);
        if result.len() >= 6 {
            break;
        }
    }
    result.sort_by_key(|r| r.seq);
    Ok(result)
}
