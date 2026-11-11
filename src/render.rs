//! Text for people: the review of a plan, the status of an approved plan, the
//! tool list, and what `resolve` recorded.
//!
//! Every function here is pure: data in, a `String` out. Every string that
//! came from a plan, a program, or a server passes through [`escape`] or is
//! printed as JSON with `\u` escapes, so a value can never move the cursor,
//! change colors, reorder text, or start a new output line.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::Value;

use crate::mcp::ToolClass;
use crate::plan_file::{
    PlanFile, PlannedTool, PlannedWrite, ServerFacts, class_words, printable_json,
};
use crate::recovery::Resolution;
use crate::status::{
    NextAction, PlanState, ServerFileState, StatusReport, WriteState, WriteStatus, grouped, plural,
};

/// Rows of a tool's table, and identities of a list, shown without `--full`.
const SHOWN: usize = 20;
/// Characters of one value before it is shortened.
const CELL: usize = 60;
/// Characters of an error text before it is shortened.
const ERROR_TEXT: usize = 240;
/// The hint under a section of writes a person can settle.
const SKIP_HINT: &str = "Use --as skipped for a write that must not be sent.";
/// Longest example value of an argument that differs between writes.
const EXAMPLE: usize = 24;
/// Longest line of a wrapped list of identities.
const LIST_WIDTH: usize = 96;

/// Whether printing `character` could change what a person sees beyond the
/// character itself: a control character (C0, DEL, C1), a line or paragraph
/// separator, a format character (general category Cf, which holds the
/// bidirectional controls), or a character that is drawn as nothing: a
/// zero-width character, a filler, a blank pattern, or a variation selector.
///
/// ```
/// use codemode::render::unsafe_to_print;
///
/// assert!(unsafe_to_print('\u{1b}') && unsafe_to_print('\n') && unsafe_to_print('\u{9b}'));
/// assert!(unsafe_to_print('\u{202e}') && unsafe_to_print('\u{200b}'));
/// assert!(unsafe_to_print('\u{3164}') && unsafe_to_print('\u{fe0f}'));
/// assert!(!unsafe_to_print('a') && !unsafe_to_print('é') && !unsafe_to_print('北'));
/// ```
#[must_use]
pub fn unsafe_to_print(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            // General category Cf.
            '\u{ad}'
                | '\u{600}'..='\u{605}'
                | '\u{61c}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0001}'
                | '\u{e0020}'..='\u{e007f}'
                // Line and paragraph separators.
                | '\u{2028}'..='\u{2029}'
                // Drawn as nothing: joiners, fillers, a blank pattern, and
                // variation selectors.
                | '\u{34f}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180d}'
                | '\u{180f}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{ffa0}'
                | '\u{e0100}'..='\u{e01ef}'
        )
}

/// `text` with every character that is [`unsafe_to_print`] written as a
/// visible escape such as `\u{1b}`. The result is one line.
///
/// ```
/// use codemode::render::escape;
///
/// assert_eq!(escape("plain é 北"), "plain é 北");
/// assert_eq!(escape("a\u{1b}[31mb\nc"), r"a\u{1b}[31mb\u{a}c");
/// assert_eq!(escape("abc\u{202e}def"), r"abc\u{202e}def");
/// ```
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if unsafe_to_print(character) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        } else {
            out.push(character);
        }
    }
    out
}

/// JSON text with every character that is [`unsafe_to_print`] written as a
/// JSON `\u` escape. Such a character can only stand inside a string, so the
/// result is JSON for the same value. Line feeds between values are kept.
///
/// ```
/// use codemode::render::escape_json;
///
/// let text = serde_json::json!({"text": "a\u{202e}b\u{9b}c"}).to_string();
/// let safe = escape_json(&text);
/// assert_eq!(safe, r#"{"text":"a\u202eb\u009bc"}"#);
/// assert_eq!(serde_json::from_str::<serde_json::Value>(&safe).unwrap()["text"], "a\u{202e}b\u{9b}c");
/// ```
#[must_use]
pub fn escape_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for character in json.chars() {
        if character != '\n' && unsafe_to_print(character) {
            for unit in character.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        } else {
            out.push(character);
        }
    }
    out
}

/// `value` as JSON on one line, cut to about `limit` characters. A cut value
/// ends in ` ...` and gives its whole size in bytes.
fn shortened(value: &Value, limit: usize) -> String {
    let full = printable_json(value);
    let characters: Vec<char> = full.chars().collect();
    if characters.len() <= limit {
        return full;
    }
    let budget = limit.saturating_sub(4);
    let mut out = String::new();
    let mut used = 0;
    let mut at = 0;
    while at < characters.len() {
        // Never cut inside an escape.
        let size = match (characters[at], characters.get(at + 1)) {
            ('\\', Some('u')) => 6,
            ('\\', Some(_)) => 2,
            _ => 1,
        };
        if used + size > budget {
            break;
        }
        out.extend(&characters[at..(at + size).min(characters.len())]);
        used += size;
        at += size;
    }
    let (closing, bytes) = match value {
        Value::String(text) => ("\"", text.len()),
        _ => ("", full.len()),
    };
    format!(
        "{} ...{closing} ({})",
        out.trim_end(),
        plural(bytes, "byte")
    )
}

/// What a review shows besides the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewOptions {
    /// Print every write with every argument in full.
    pub full: bool,
    /// Writes in flight at once, as the approval will bind it.
    pub concurrency: usize,
    /// Times one write may be sent, as the approval will bind it.
    pub max_attempts: u32,
}

/// Characters of a summary shown before it is cut.
const SUMMARY: usize = 200;

/// A summary on one line: every run of white space becomes one space, so
/// that padding cannot push text onto lines of its own, and a long summary
/// is cut and shown with its size.
fn summary_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY {
        return escape(&collapsed);
    }
    let cut: String = collapsed.chars().take(SUMMARY).collect();
    format!(
        "{} ... ({})",
        escape(cut.trim_end()),
        plural(text.len(), "byte")
    )
}

/// The first lines of a review or a status. `review` is set in a review, to
/// whether everything is printed in full: a review also shows the protocol
/// revision and the summary.
fn header(out: &mut String, report: &StatusReport, review: Option<bool>) {
    let _ = writeln!(
        out,
        "{}  sha256:{}",
        escape(&report.file),
        report.digest.short()
    );
    let server = &report.server;
    let _ = write!(out, "server   {}", escape(&server.identity));
    if review.is_some() {
        let _ = write!(out, "  (MCP {})", escape(&server.protocol));
    }
    out.push('\n');
    let file = server
        .file
        .as_deref()
        .map_or_else(|| "(no path recorded)".to_string(), escape);
    let _ = writeln!(
        out,
        "config   {file}  {}",
        match server.file_state {
            ServerFileState::Unreadable => "cannot be read".to_string(),
            ServerFileState::Changed => "changed since the plan was made".to_string(),
            ServerFileState::Same | ServerFileState::Unchecked =>
                format!("sha256:{}", server.config.short()),
        }
    );
    // A shortened identity hides part of a command that will be executed.
    if let Some(command) = &server.command {
        let _ = writeln!(out, "command  {}", escape(command));
    }
    if let (Some(text), Some(full)) = (&report.summary, review) {
        let _ = writeln!(
            out,
            "summary  {}",
            if full {
                // Nothing is dropped: the summary exactly as the plan holds it.
                printable_json(&Value::String(text.clone()))
            } else {
                summary_line(text)
            }
        );
    }
    let _ = writeln!(out, "status   {}", report.detail);
    if let Some(approval) = &report.approval {
        let _ = writeln!(
            out,
            "         approved by {} on {}",
            escape(&approval.approver),
            approval.approved_at
        );
    }
}

/// The writes of each tool, in the order a review shows the tools: marked
/// destructive first, then without annotations, then by number of writes.
fn by_tool(plan: &PlanFile) -> Vec<(&String, &PlannedTool, Vec<&PlannedWrite>)> {
    let mut writes: BTreeMap<&str, Vec<&PlannedWrite>> = BTreeMap::new();
    for write in &plan.writes {
        writes.entry(write.tool.as_str()).or_default().push(write);
    }
    let mut groups: Vec<_> = plan
        .tools
        .iter()
        .filter_map(|(name, tool)| Some((name, tool, writes.remove(name.as_str())?)))
        .collect();
    groups.sort_by(|a, b| {
        (!a.1.destructive, a.1.annotated, b.2.len(), a.0).cmp(&(
            !b.1.destructive,
            b.1.annotated,
            a.2.len(),
            b.0,
        ))
    });
    groups
}

/// The review of a plan: what a person reads before approving it.
///
/// `report` gives the header (file, digest, server, status); `plan` gives the
/// writes. Writes are grouped by tool. For each tool the review says which
/// arguments are the same in every write and which differ, then lists the
/// writes: at most 20 per tool, each value cut to about 60 characters, unless
/// `options.full` is set. Whatever is hidden or cut is announced.
#[must_use]
pub fn review(report: &StatusReport, plan: &PlanFile, options: ReviewOptions) -> String {
    let mut out = String::new();
    header(&mut out, report, Some(options.full));
    let groups = by_tool(plan);
    let total = plan.writes.len();
    let _ = writeln!(
        out,
        "\n{} with {}",
        plural(total, "write"),
        plural(groups.len(), "tool")
    );
    for (name, tool, writes) in &groups {
        out.push('\n');
        let mut title = format!("{}  {}", escape(name), plural(writes.len(), "write"));
        title.push_str(if tool.repeatable() {
            "  repeatable"
        } else {
            "  not repeatable"
        });
        if tool.destructive {
            title.push_str("  destructive");
        }
        if !tool.annotated {
            title.push_str("  no annotations");
        }
        let _ = writeln!(out, "{title}");
        if total <= 2 {
            for write in writes {
                one_write(&mut out, write, false);
            }
        } else if let [write] = writes.as_slice() {
            one_write(&mut out, write, !options.full);
        } else {
            group(&mut out, writes, options.full);
        }
    }
    notes(&mut out, plan, &groups, options.full);
    out.push('\n');
    let repeatable = plan.tools.values().any(PlannedTool::repeatable);
    let _ = writeln!(
        out,
        "{}",
        if repeatable {
            format!(
                "If a reply is lost: a repeatable write is sent again (up to {} attempts). \
                 Any other write waits for a person.",
                options.max_attempts
            )
        } else {
            "If a reply is lost: the write is not sent again. It waits for a person.".to_string()
        }
    );
    let _ = writeln!(
        out,
        "Sending: the first write alone, then up to {} at a time. A failed write stops the rest.",
        options.concurrency
    );
    let _ = writeln!(out, "Next: codemode apply {}", escape(&report.file));
    out
}

/// An object's entries with sorted keys.
fn sorted(arguments: &Value) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = arguments
        .as_object()
        .map(|map| map.iter().collect())
        .unwrap_or_default();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// One write as its identity and its arguments in canonical JSON, never cut.
fn block(out: &mut String, write: &PlannedWrite) {
    let _ = writeln!(
        out,
        "  {}\n    {}",
        escape(&write.id),
        printable_json(&write.arguments)
    );
}

/// One write with every argument on its own line. With `cut`, a long value
/// is cut and shows its whole size, as in a table.
fn one_write(out: &mut String, write: &PlannedWrite, cut: bool) {
    let _ = writeln!(out, "  {}", escape(&write.id));
    let entries = sorted(&write.arguments);
    if entries.is_empty() {
        out.push_str("    (no arguments)\n");
    }
    let keys: Vec<String> = entries.iter().map(|(key, _)| escape(key)).collect();
    let width = keys
        .iter()
        .map(|key| key.chars().count())
        .max()
        .unwrap_or(0);
    for (key, (_, value)) in keys.iter().zip(&entries) {
        let value = if cut {
            shortened(value, CELL)
        } else {
            printable_json(value)
        };
        let _ = writeln!(out, "    {}  {value}", pad(key, width));
    }
}

fn pad(text: &str, width: usize) -> String {
    let length = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(length)))
}

/// The writes of one tool: what is the same, what differs, and the writes.
fn group(out: &mut String, writes: &[&PlannedWrite], full: bool) {
    let mut keys: Vec<&String> = writes
        .iter()
        .flat_map(|write| {
            write
                .arguments
                .as_object()
                .into_iter()
                .flat_map(|map| map.keys())
        })
        .collect();
    keys.sort();
    keys.dedup();
    let mut same = Vec::new();
    let mut different = Vec::new();
    for key in keys {
        let mut distinct: Vec<&Value> = Vec::new();
        let mut seen = HashSet::new();
        let mut absent = 0;
        for write in writes {
            match write.arguments.get(key.as_str()) {
                Some(value) => {
                    if seen.insert(printable_json(value)) {
                        distinct.push(value);
                    }
                }
                None => absent += 1,
            }
        }
        if absent == 0 && distinct.len() == 1 {
            same.push(format!("{}={}", escape(key), shortened(distinct[0], CELL)));
            continue;
        }
        let mut line = format!("{}: {}", escape(key), plural(distinct.len(), "value"));
        let examples: Vec<String> = distinct.iter().take(4).map(|v| printable_json(v)).collect();
        let short = distinct.iter().take(4).zip(&examples).all(|(value, text)| {
            !value.is_array() && !value.is_object() && text.chars().count() <= EXAMPLE
        });
        if short {
            let more = if distinct.len() > examples.len() {
                ", ..."
            } else {
                ""
            };
            let _ = write!(line, " ({}{more})", examples.join(", "));
        }
        if absent > 0 {
            let _ = write!(line, ", absent in {}", plural(absent, "write"));
        }
        different.push((key, line));
    }
    const LABEL: usize = 22;
    if !same.is_empty() {
        let _ = writeln!(
            out,
            "  {}{}",
            pad("same in every write", LABEL),
            same.join("  ")
        );
    }
    if different.is_empty() {
        let _ = writeln!(
            out,
            "  {}nothing: every write has the same arguments",
            pad("different", LABEL)
        );
    }
    for (index, (_, line)) in different.iter().enumerate() {
        let label = if index == 0 { "different" } else { "" };
        let _ = writeln!(out, "  {}{line}", pad(label, LABEL));
    }
    out.push('\n');
    if full {
        for write in writes {
            block(out, write);
        }
        return;
    }
    let shown = &writes[..writes.len().min(SHOWN)];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(shown.len() + 1);
    // An argument may itself be named `id`; the first column then says what
    // it holds.
    let named_id = different.iter().any(|(key, _)| key.as_str() == "id");
    let mut titles = vec![if named_id { "write" } else { "id" }.to_string()];
    titles.extend(different.iter().map(|(key, _)| escape(key)));
    rows.push(titles);
    for write in shown {
        let mut row = vec![escape(&write.id)];
        for (key, _) in &different {
            row.push(match write.arguments.get(key.as_str()) {
                Some(value) => shortened(value, CELL),
                None => "(absent)".into(),
            });
        }
        rows.push(row);
    }
    table(out, &rows);
    if writes.len() > shown.len() {
        let _ = writeln!(
            out,
            "  ... {} more (--full)",
            grouped(writes.len() - shown.len())
        );
    }
}

/// Rows as columns two spaces apart, indented by two.
fn table(out: &mut String, rows: &[Vec<String>]) {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        let mut line = String::from(" ");
        for (index, cell) in row.iter().enumerate() {
            line.push(' ');
            line.push_str(&if index + 1 == row.len() {
                cell.clone()
            } else {
                format!("{} ", pad(cell, widths[index]))
            });
        }
        let _ = writeln!(out, "{}", line.trim_end());
    }
}

/// Identities joined by commas and wrapped, at most 20 unless `full`.
fn id_list(out: &mut String, indent: &str, ids: &[&str], full: bool) {
    let shown = if full {
        ids.len()
    } else {
        ids.len().min(SHOWN)
    };
    let mut line = String::new();
    for (index, id) in ids[..shown].iter().enumerate() {
        let mut piece = escape(id);
        if index + 1 < shown {
            piece.push(',');
        }
        if !line.is_empty() && line.chars().count() + 1 + piece.chars().count() > LIST_WIDTH {
            let _ = writeln!(out, "{indent}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&piece);
    }
    if !line.is_empty() {
        let _ = writeln!(out, "{indent}{line}");
    }
    if ids.len() > shown {
        let _ = writeln!(
            out,
            "{indent}... {} more (--full)",
            grouped(ids.len() - shown)
        );
    }
}

fn notes(
    out: &mut String,
    plan: &PlanFile,
    groups: &[(&String, &PlannedTool, Vec<&PlannedWrite>)],
    full: bool,
) {
    let mut lines: Vec<String> = Vec::new();
    let names = |filter: &dyn Fn(&PlannedTool) -> bool| -> (Vec<String>, usize) {
        let chosen: Vec<_> = groups.iter().filter(|group| filter(group.1)).collect();
        (
            chosen.iter().map(|group| escape(group.0)).collect(),
            chosen.iter().map(|group| group.2.len()).sum(),
        )
    };
    let (bare, _) = names(&|tool| !tool.annotated);
    if !bare.is_empty() {
        lines.push(format!(
            "! {} no annotations from the server: {}. {} writes are not sent again after a \
             lost reply.",
            if bare.len() == 1 {
                "1 tool has".to_string()
            } else {
                format!("{} tools have", bare.len())
            },
            bare.join(", "),
            if bare.len() == 1 { "Its" } else { "Their" }
        ));
    }
    let (destructive, writes) = names(&|tool| tool.destructive);
    if !destructive.is_empty() {
        lines.push(format!(
            "! {} a tool the server marks destructive: {}.",
            if writes == 1 {
                "1 write uses".to_string()
            } else {
                format!("{} writes use", grouped(writes))
            },
            destructive.join(", ")
        ));
    }
    let identical = plan.identical_writes();
    let mut identical_ids = String::new();
    if !identical.is_empty() {
        let count: usize = identical.iter().map(Vec::len).sum();
        lines.push(format!(
            "! {} writes are identical to another write (same tool, same arguments):",
            grouped(count)
        ));
        let ids: Vec<&str> = identical.into_iter().flatten().collect();
        id_list(&mut identical_ids, "    ", &ids, full);
    }
    if plan.server.trust_annotations {
        lines.push("Tool classes come from the server's own hints.".into());
    }
    if lines.is_empty() {
        return;
    }
    out.push_str("\nNotes\n");
    for line in lines {
        let _ = writeln!(out, "  {line}");
        if line.ends_with("(same tool, same arguments):") {
            out.push_str(&identical_ids);
        }
    }
}

/// The question `apply` asks on a terminal before the first write. It names
/// the number of writes and the server, and says first when a tool is marked
/// destructive or has no annotations.
#[must_use]
pub fn question(plan: &PlanFile) -> String {
    let mut out = String::new();
    let writes_of = |filter: &dyn Fn(&PlannedTool) -> bool| {
        plan.writes
            .iter()
            .filter(|write| plan.tools.get(&write.tool).is_some_and(filter))
            .count()
    };
    let destructive = writes_of(&|tool| tool.destructive);
    if destructive > 0 {
        let _ = writeln!(
            out,
            "{} a tool the server marks destructive.",
            if destructive == 1 {
                "1 write uses".to_string()
            } else {
                format!("{} writes use", grouped(destructive))
            }
        );
    }
    let bare = writes_of(&|tool| !tool.annotated);
    if bare > 0 {
        let _ = writeln!(
            out,
            "{} a tool without annotations from the server.",
            if bare == 1 {
                "1 write uses".to_string()
            } else {
                format!("{} writes use", grouped(bare))
            }
        );
    }
    let count = plan.writes.len();
    let _ = write!(
        out,
        "Apply {} through {}? Type \"yes\" to approve: ",
        if count == 1 {
            "this 1 write".to_string()
        } else {
            format!("these {} writes", grouped(count))
        },
        escape(&plan.server.identity)
    );
    out
}

/// The status of a plan: the review while it is not approved, and afterwards
/// what happened to each write and the exact command to run next.
///
/// Sections appear only when they have writes, in this order: failed, no
/// reply, refused, resolved by a person, skipped by a person, not sent,
/// applied. Lists of
/// identities stop at 20 unless `options.full` is set, and say how many they
/// leave out.
#[must_use]
pub fn status(report: &StatusReport, plan: &PlanFile, options: ReviewOptions) -> String {
    match report.state {
        PlanState::NotApproved => return review(report, plan, options),
        PlanState::Edited => return edited(report),
        _ => {}
    }
    let full = options.full;
    let file = escape(&report.file);
    let mut out = String::new();
    header(&mut out, report, None);
    let counts = &report.counts;
    let _ = write!(
        out,
        "\napplied {} · failed {} · no reply {} · not sent {}",
        grouped(counts.applied),
        grouped(counts.failed),
        grouped(counts.no_reply),
        grouped(counts.not_sent)
    );
    if counts.refused > 0 {
        let _ = write!(out, " · refused {}", grouped(counts.refused));
    }
    if counts.resolved > 0 {
        let _ = write!(out, " · resolved {}", grouped(counts.resolved));
    }
    if counts.skipped > 0 {
        let _ = write!(out, " · skipped {}", grouped(counts.skipped));
    }
    out.push('\n');
    let of = |state: WriteState| -> Vec<&WriteStatus> {
        report
            .writes
            .iter()
            .filter(|write| write.state == state)
            .collect()
    };

    let failed = of(WriteState::Failed);
    if !failed.is_empty() {
        let _ = writeln!(
            out,
            "\nfailed ({}): the server answered with an error",
            grouped(failed.len())
        );
        let groups = problem_groups(&mut out, &failed, full);
        unknown_advice(
            &mut out,
            &failed,
            groups,
            &file,
            "--errors",
            "codemode cannot tell whether a failed write changed anything.",
            report,
        );
    }
    let silent = of(WriteState::NoReply);
    if !silent.is_empty() {
        let _ = writeln!(
            out,
            "\nno reply ({}): the write may have reached the server, and no answer came back",
            grouped(silent.len())
        );
        let groups = problem_groups(&mut out, &silent, full);
        unknown_advice(
            &mut out,
            &silent,
            groups,
            &file,
            "--no-reply",
            "The write may or may not have happened.",
            report,
        );
    }
    let refused = of(WriteState::Refused);
    if !refused.is_empty() {
        let _ = writeln!(
            out,
            "\nrefused ({}): not applied, and not sent again",
            grouped(refused.len())
        );
        let groups = problem_groups(&mut out, &refused, full);
        let indent = if groups == 1 { "    " } else { "  " };
        if groups > 1 {
            out.push('\n');
        }
        let (again, spent): (Vec<&WriteStatus>, Vec<&WriteStatus>) = refused
            .iter()
            .partition(|write| write.next == NextAction::Resolve);
        if !again.is_empty() {
            let _ = writeln!(
                out,
                "{indent}The server refused {} before running it. Fix the cause, then run:",
                if again.len() == 1 && spent.is_empty() {
                    "this write".to_string()
                } else {
                    plural(again.len(), "write")
                }
            );
            let ids: Vec<String> = again.iter().take(10).map(|w| escape(&w.id)).collect();
            let more = if again.len() > ids.len() { " ..." } else { "" };
            let _ = writeln!(
                out,
                "{indent}  codemode resolve {file} {}{more} --as not-applied   (apply then sends it again)",
                ids.join(" ")
            );
        }
        if !spent.is_empty() {
            let _ = writeln!(
                out,
                "{indent}Every attempt of {} is used. Make a new plan for {} with `codemode plan`.",
                plural(spent.len(), "write"),
                if spent.len() == 1 { "it" } else { "them" }
            );
        }
        let _ = writeln!(out, "{indent}{SKIP_HINT}");
    }
    let resolved = of(WriteState::Resolved);
    if !resolved.is_empty() {
        let _ = writeln!(
            out,
            "\nresolved by a person ({}): recorded as applied",
            grouped(resolved.len())
        );
        let ids: Vec<&str> = resolved.iter().map(|write| write.id.as_str()).collect();
        id_list(&mut out, "  ", &ids, full);
    }
    let skipped = of(WriteState::Skipped);
    if !skipped.is_empty() {
        let _ = writeln!(
            out,
            "\nskipped by a person ({}): not applied, and never sent",
            grouped(skipped.len())
        );
        let ids: Vec<&str> = skipped.iter().map(|write| write.id.as_str()).collect();
        id_list(&mut out, "  ", &ids, full);
    }
    let waiting = of(WriteState::NotSent);
    if !waiting.is_empty() {
        let _ = writeln!(out, "\nnot sent ({})", grouped(waiting.len()));
        let _ = writeln!(
            out,
            "  {}",
            match report.state {
                PlanState::Running => format!(
                    "A session may still be running. If none is, run `codemode apply {file}` to continue."
                ),
                PlanState::Approved => format!("Run `codemode apply {file}` to start."),
                _ => format!("Run `codemode apply {file}` to continue."),
            }
        );
    }
    let applied = of(WriteState::Applied);
    if !applied.is_empty() {
        let _ = writeln!(out, "\napplied ({})", grouped(applied.len()));
        let ids: Vec<&str> = applied.iter().map(|write| write.id.as_str()).collect();
        id_list(&mut out, "  ", &ids, full);
    }
    out
}

/// Why a write did not end applied, in the server's words when the journal
/// kept them.
fn reason(write: &WriteStatus, full: bool) -> String {
    if let Some(text) = &write.error_text {
        return error_line(text, full);
    }
    match write.error_code.as_deref() {
        Some("no_result") => {
            "no answer is recorded: codemode is still waiting for it, or stopped before it came"
                .into()
        }
        Some("deadline") => "the deadline passed before an answer came back".into(),
        Some("cancelled") => "cancelled before an answer came back".into(),
        Some(code) => format!("no error text was kept (code {})", escape(code)),
        None => "no error text was kept".into(),
    }
}

/// A server's error text on one line, without JSON quoting: `"` and `\` are
/// printed as they are, and every character that is [`unsafe_to_print`] is a
/// visible escape such as `\u{a}`, so the text never starts a line of its
/// own. Unless `full`, each run of spaces becomes one space, so padding
/// cannot push text to where a line of its own would begin, and a text longer
/// than about 240 characters is cut, never inside an escape, and shows its
/// whole size in bytes.
fn error_line(text: &str, full: bool) -> String {
    let budget = ERROR_TEXT.saturating_sub(4);
    let mut out = String::with_capacity(text.len());
    // Characters in `out`, and where `out` ends at the last piece that fits
    // the budget.
    let mut shown = 0;
    let mut cut = None;
    let mut space = false;
    for character in text.chars() {
        let unsafe_character = unsafe_to_print(character);
        if !full && character.is_whitespace() && !unsafe_character {
            // Leading spaces are dropped, and trailing ones never written.
            space = !out.is_empty();
            continue;
        }
        let before = out.len();
        if std::mem::take(&mut space) {
            out.push(' ');
        }
        if unsafe_character {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        } else {
            out.push(character);
        }
        let size = out[before..].chars().count();
        if cut.is_none() && shown + size > budget {
            cut = Some(before);
        }
        shown += size;
    }
    if out.is_empty() {
        return "(the error text is empty)".into();
    }
    match cut {
        Some(at) if !full && shown > ERROR_TEXT => format!(
            "{} ... ({})",
            out[..at].trim_end(),
            plural(text.len(), "byte")
        ),
        _ => out,
    }
}

/// Print `writes` grouped by identical reason, the reason once per group: at
/// most 20 groups unless `full`. Returns the number of groups.
fn problem_groups(out: &mut String, writes: &[&WriteStatus], full: bool) -> usize {
    let mut groups: Vec<(String, Vec<&WriteStatus>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for write in writes {
        let text = reason(write, full);
        match index.get(&text) {
            Some(&at) => groups[at].1.push(write),
            None => {
                index.insert(text.clone(), groups.len());
                groups.push((text, vec![write]));
            }
        }
    }
    let shown = if full {
        groups.len()
    } else {
        groups.len().min(SHOWN)
    };
    for (text, members) in &groups[..shown] {
        let mut tools: Vec<&str> = members.iter().map(|write| write.tool.as_str()).collect();
        tools.sort_unstable();
        tools.dedup();
        let tool = match tools.as_slice() {
            [tool] => escape(tool),
            many => plural(many.len(), "tool"),
        };
        if let [write] = members.as_slice() {
            let _ = writeln!(out, "  {}  {tool}\n    {text}", escape(&write.id));
        } else {
            let _ = writeln!(
                out,
                "  {}  {tool}\n    {text}",
                plural(members.len(), "write")
            );
            let ids: Vec<&str> = members.iter().map(|write| write.id.as_str()).collect();
            id_list(out, "    ", &ids, full);
        }
    }
    if groups.len() > shown {
        let hidden: usize = groups[shown..]
            .iter()
            .map(|(_, members)| members.len())
            .sum();
        let _ = writeln!(
            out,
            "  ... {} more {} with {} (--full)",
            grouped(hidden),
            if hidden == 1 { "write" } else { "writes" },
            plural(groups.len() - shown, "other error text")
        );
    }
    groups.len()
}

/// What to do about writes whose outcome is unknown.
fn unknown_advice(
    out: &mut String,
    writes: &[&WriteStatus],
    groups: usize,
    file: &str,
    selector: &str,
    why: &str,
    report: &StatusReport,
) {
    let indent = if groups == 1 { "    " } else { "  " };
    if groups > 1 {
        out.push('\n');
    }
    let person: Vec<&&WriteStatus> = writes
        .iter()
        .filter(|write| write.next == NextAction::Resolve)
        .collect();
    let again = writes.len() - person.len();
    if again > 0 {
        let attempts = report.approval.as_ref().map_or(String::new(), |approval| {
            format!(" (up to {} attempts each)", approval.max_attempts)
        });
        let _ = writeln!(
            out,
            "{indent}{} repeatable: `codemode apply {file}` sends {} again{attempts}.",
            if again == 1 {
                "1 write is".to_string()
            } else {
                format!("{} writes are", grouped(again))
            },
            if again == 1 { "it" } else { "them" }
        );
    }
    if person.is_empty() {
        return;
    }
    let target = match person.as_slice() {
        [write] if again == 0 => escape(&write.id),
        _ => selector.to_string(),
    };
    let _ = writeln!(out, "{indent}{why} Check the target, then run:");
    let _ = writeln!(
        out,
        "{indent}  codemode resolve {file} {target} --as not-applied   (or --as applied)"
    );
    let _ = writeln!(out, "{indent}{SKIP_HINT}");
}

fn edited(report: &StatusReport) -> String {
    let mut out = String::new();
    header(&mut out, report, None);
    let approved = report
        .approval
        .as_ref()
        .and_then(|approval| approval.digest.as_ref())
        .map_or_else(
            || "its journal was not made by `codemode apply`".to_string(),
            |digest| format!("the approved plan is sha256:{}", digest.short()),
        );
    let _ = writeln!(
        out,
        "\nThis file is not the plan that was approved: {approved}.\n\
         codemode sends nothing from an edited plan. Restore the approved file to continue, or\n\
         to see what happened to each write. To apply the edited plan as a new plan, save it\n\
         under another name. Every write in it is then sent, also one that was applied before."
    );
    out
}

/// The tool list of a server: each tool's class, what the server marks, and
/// whether a plan may hold it.
#[must_use]
pub fn tools(facts: &ServerFacts) -> String {
    let mut out = format!(
        "server   {}  (MCP {})\n\n",
        escape(&facts.identity),
        escape(&facts.protocol)
    );
    let mut rows = vec![vec![
        "tool".to_string(),
        "class".to_string(),
        "server marks it".to_string(),
        "in a plan".to_string(),
    ]];
    let mut writes = 0;
    let mut allowed = 0;
    for (name, (tool, class)) in &facts.tools {
        let hints = tool.annotations;
        let marks = if hints == crate::mcp::Annotations::default() {
            "no annotations"
        } else if hints.destructive_hint == Some(true) && *class != ToolClass::Read {
            "destructive"
        } else {
            "-"
        };
        let plan = if *class == ToolClass::Read {
            "no: a program may call it"
        } else {
            writes += 1;
            if facts.allows_write(name) {
                allowed += 1;
                "yes"
            } else {
                "no: not in write_tools"
            }
        };
        rows.push(vec![
            escape(name),
            class_words(*class).to_string(),
            marks.to_string(),
            plan.to_string(),
        ]);
    }
    table(&mut out, &rows);
    let mut lines = Vec::new();
    if !facts.trust_annotations {
        lines.push(
            if facts
                .tools
                .values()
                .any(|(_, class)| *class == ToolClass::Read)
            {
                "trust_annotations is off: every tool outside read_tools is a write, and no \
                 write is repeatable."
                    .to_string()
            } else {
                "trust_annotations is off: every tool is a write; set trust_annotations or \
                 read_tools to let a program read."
                    .to_string()
            },
        );
    }
    if facts.write_tools.is_some() {
        lines.push(format!(
            "write_tools allows {allowed} of {} in a plan.",
            plural(writes, "write tool")
        ));
    }
    if !lines.is_empty() {
        out.push('\n');
    }
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// What `resolve` is about to record: the writes it selected, grouped by the
/// reason each one did not end applied.
#[must_use]
pub fn resolution(selected: &[&WriteStatus], resolution: Resolution, reviewer: &str) -> String {
    let mut out = format!(
        "Recording {} as {} (reviewer {}):\n\n",
        plural(selected.len(), "write"),
        match resolution {
            Resolution::Applied => "applied",
            Resolution::NotApplied => "not applied",
            Resolution::Skipped => "skipped",
        },
        escape(reviewer)
    );
    problem_groups(&mut out, selected, true);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp::Tool;
    use crate::plan::Digest;
    use crate::plan_file::PlanServer;
    use crate::status::{ApprovalStatus, Counts, ServerStatus};

    const OPTIONS: ReviewOptions = ReviewOptions {
        full: false,
        concurrency: 4,
        max_attempts: 3,
    };

    fn planned(repeatable: bool, destructive: bool, annotated: bool) -> PlannedTool {
        PlannedTool {
            class: if repeatable {
                ToolClass::RepeatableWrite
            } else {
                ToolClass::Write
            },
            definition: Digest::of(b"definition"),
            destructive,
            annotated,
        }
    }

    fn plan_of(
        tools: &[(&str, PlannedTool)],
        writes: Vec<Value>,
        summary: Option<&str>,
    ) -> PlanFile {
        PlanFile {
            summary: summary.map(String::from),
            server: PlanServer {
                identity: "https://mcp.example.com/mcp".into(),
                file: Some("server.json".into()),
                config: Digest::of(b"server file"),
                protocol: "2025-11-25".into(),
                trust_annotations: true,
            },
            tools: tools
                .iter()
                .map(|(name, tool)| ((*name).to_string(), tool.clone()))
                .collect(),
            writes: writes
                .into_iter()
                .map(|write| serde_json::from_value(write).expect("write"))
                .collect(),
        }
    }

    fn report_of(plan: &PlanFile) -> StatusReport {
        let mut report = StatusReport::new(
            "triage.plan.json",
            plan,
            &plan.frozen().expect("valid"),
            None,
        );
        report.digest = Digest::of(b"fixed");
        report
    }

    fn reviewed(plan: &PlanFile, full: bool) -> String {
        review(&report_of(plan), plan, ReviewOptions { full, ..OPTIONS })
    }

    #[test]
    fn unsafe_characters_become_visible_escapes() {
        assert_eq!(escape(""), "");
        assert_eq!(escape("tab\there"), r"tab\u{9}here");
        assert_eq!(escape("\u{1b}[2J\u{1b}[H"), r"\u{1b}[2J\u{1b}[H");
        assert_eq!(escape("del\u{7f} c1\u{9b}31m"), r"del\u{7f} c1\u{9b}31m");
        assert_eq!(
            escape("line\r\nbreak\u{2028}x\u{2029}"),
            r"line\u{d}\u{a}break\u{2028}x\u{2029}"
        );
        assert_eq!(
            escape("bidi \u{202e}evil\u{202c} \u{2066}x\u{2069} \u{200f}\u{61c}"),
            r"bidi \u{202e}evil\u{202c} \u{2066}x\u{2069} \u{200f}\u{61c}"
        );
        assert_eq!(
            escape("zero\u{200b}width\u{200d}\u{feff}\u{2060}"),
            r"zero\u{200b}width\u{200d}\u{feff}\u{2060}"
        );
        assert_eq!(escape("tag\u{e0041}"), r"tag\u{e0041}");
        // Ordinary text in any script is left alone.
        let plain = "naïve café — 日本語 🙂 «quotes» a\\b";
        assert_eq!(escape(plain), plain);
        for character in ('\0'..='\u{ffff}').chain('\u{e0000}'..='\u{e007f}') {
            let escaped = escape(&character.to_string());
            assert!(
                escaped.chars().all(|c| !unsafe_to_print(c)),
                "U+{:04X} survives",
                u32::from(character)
            );
        }
    }

    #[test]
    fn invisible_and_format_characters_are_unsafe_to_print() {
        // General category Cf, and characters that are drawn as nothing.
        let ranges: &[(u32, u32)] = &[
            (0xad, 0xad),
            (0x34f, 0x34f),
            (0x600, 0x605),
            (0x61c, 0x61c),
            (0x6dd, 0x6dd),
            (0x70f, 0x70f),
            (0x890, 0x891),
            (0x8e2, 0x8e2),
            (0x115f, 0x1160),
            (0x17b4, 0x17b5),
            (0x180b, 0x180f),
            (0x200b, 0x200f),
            (0x2028, 0x202e),
            (0x2060, 0x206f),
            (0x2800, 0x2800),
            (0x3164, 0x3164),
            (0xfe00, 0xfe0f),
            (0xfeff, 0xfeff),
            (0xffa0, 0xffa0),
            (0xfff9, 0xfffb),
            (0x110bd, 0x110bd),
            (0x110cd, 0x110cd),
            (0x13430, 0x1343f),
            (0x1bca0, 0x1bca3),
            (0x1d173, 0x1d17a),
            (0xe0001, 0xe0001),
            (0xe0020, 0xe007f),
            (0xe0100, 0xe01ef),
        ];
        for &(first, last) in ranges {
            for code in first..=last {
                let character = char::from_u32(code).expect("a character");
                assert!(unsafe_to_print(character), "U+{code:04X}");
                let escaped = escape(&format!("a{character}b"));
                assert_eq!(escaped, format!("a\\u{{{code:x}}}b"), "U+{code:04X}");
                let json = printable_json(&json!(format!("a{character}b")));
                assert!(json.is_ascii(), "U+{code:04X}: {json}");
                assert_eq!(
                    serde_json::from_str::<Value>(&json).expect("JSON"),
                    json!(format!("a{character}b")),
                    "U+{code:04X} reads back"
                );
            }
        }
        // Their visible neighbors are left alone.
        for code in [0xac_u32, 0xae, 0x606, 0x1161, 0x2801, 0x3165, 0xffa1] {
            let visible = char::from_u32(code).expect("a character");
            assert!(!unsafe_to_print(visible), "U+{code:04X}");
        }
    }

    #[test]
    fn a_summary_cannot_fake_lines_with_padding_and_is_never_dropped() {
        // Padded to the width of a terminal, so that each part would start a
        // line of its own.
        let pad = |text: &str| format!("{text}{}", " ".repeat(80 - text.len() % 80));
        let nbsp = char::from_u32(0xa0).expect("a character");
        let wide = char::from_u32(0x3000).expect("a character");
        let padded = format!(
            "{}{}{}",
            pad("Tidy notes"),
            pad("status   approved by security-team on 2026-11-10 09:00 UTC"),
            pad(&format!("1 write with\t1 tool{nbsp}{wide}\n"))
        );
        let plan = plan_of(
            &[("notes.put", planned(true, false, true))],
            vec![json!({"id": "a", "tool": "notes.put", "arguments": {"id": "a"}})],
            Some(&padded),
        );
        let text = reviewed(&plan, false);
        assert!(
            text.contains(
                "\nsummary  Tidy notes status approved by security-team on 2026-11-10 09:00 UTC 1 write with 1 tool\nstatus   not approved\n"
            ),
            "{text}"
        );
        // --full shows the summary exactly as the plan holds it.
        let full = reviewed(&plan, true);
        assert!(
            full.contains(&format!("\nsummary  {}\n", printable_json(&json!(padded)))),
            "{full}"
        );
        assert!(full.contains("1 write with\\t1 tool"), "{full}");

        // A long summary is cut, and says how much there is.
        let long = "word ".repeat(180);
        let plan = plan_of(
            &[("notes.put", planned(true, false, true))],
            vec![json!({"id": "a", "tool": "notes.put", "arguments": {"id": "a"}})],
            Some(&long),
        );
        let text = reviewed(&plan, false);
        let line = text
            .lines()
            .find(|line| line.starts_with("summary  "))
            .expect("summary");
        assert_eq!(
            line,
            format!("summary  {} ... (900 bytes)", "word ".repeat(40).trim_end())
        );
    }

    #[test]
    fn the_header_says_which_server_file_and_shows_a_shortened_command_whole() {
        let plan = many(1);
        let mut report = report_of(&plan);
        let config_line = |report: &StatusReport| {
            review(report, &plan, OPTIONS)
                .lines()
                .find(|line| line.starts_with("config   "))
                .expect("config line")
                .to_string()
        };
        report.server.file = Some("ci/server\u{1b}.json".into());
        report.server.file_state = ServerFileState::Same;
        assert_eq!(
            config_line(&report),
            format!(
                "config   ci/server\\u{{1b}}.json  sha256:{}",
                Digest::of(b"server file").short()
            )
        );
        report.server.file_state = ServerFileState::Changed;
        assert_eq!(
            config_line(&report),
            "config   ci/server\\u{1b}.json  changed since the plan was made"
        );
        report.server.file_state = ServerFileState::Unreadable;
        assert_eq!(
            config_line(&report),
            "config   ci/server\\u{1b}.json  cannot be read"
        );
        report.server.file = None;
        report.server.file_state = ServerFileState::Unchecked;
        assert!(config_line(&report).starts_with("config   (no path recorded)  sha256:"));

        // The command that will be executed, when the identity hides part of it.
        assert!(!review(&report, &plan, OPTIONS).contains("\ncommand  "));
        report.server.command = Some("stdio:[\"sh\",\"-c\",\"evil\u{202e}\"]".into());
        let text = review(&report, &plan, OPTIONS);
        assert!(
            text.contains("\ncommand  stdio:[\"sh\",\"-c\",\"evil\\u{202e}\"]\nstatus   "),
            "{text}"
        );
    }

    #[test]
    fn a_long_value_is_cut_on_an_escape_boundary_with_its_size() {
        assert_eq!(shortened(&json!(101), CELL), "101");
        assert_eq!(shortened(&json!("short"), CELL), "\"short\"");
        let long = "x".repeat(212);
        assert_eq!(
            shortened(&json!(long), 20),
            format!("\"{} ...\" (212 bytes)", "x".repeat(15))
        );
        // The cut never leaves half an escape.
        let hostile = format!("{}\u{1b}\u{202e}\n{}", "a".repeat(12), "b".repeat(50));
        assert_eq!(
            shortened(&json!(hostile), 20),
            format!("\"{} ...\" (67 bytes)", "a".repeat(12))
        );
        let list = json!(["needs-triage", "bug", "help wanted", "good first issue"]);
        assert_eq!(
            shortened(&list, 30),
            "[\"needs-triage\",\"bug\",\"hel ... (55 bytes)"
        );
    }

    #[test]
    fn a_server_error_text_is_printed_without_json_quoting() {
        // Quotes and backslashes print as they are.
        let body = r#"{"successful":false,"error":"{\"message\":\"Gone\"}","path":"C:\temp"}"#;
        assert_eq!(error_line(body, false), body);
        assert_eq!(error_line(body, true), body);
        // A terminal sequence, a bidirectional control, a zero-width character,
        // and a line break are visible escapes, so the text stays on its line.
        let hostile = "no\u{1b}[2J\u{1b}[31m \u{202e}evil\u{200b}\nforged line";
        let expected = r"no\u{1b}[2J\u{1b}[31m \u{202e}evil\u{200b}\u{a}forged line";
        for full in [false, true] {
            let line = error_line(hostile, full);
            assert_eq!(line, expected);
            assert!(line.chars().all(|c| !unsafe_to_print(c)), "{line:?}");
        }
        // Without --full, padding collapses to one space, so it cannot push
        // text to the start of a terminal line. --full keeps every space.
        let padded = format!("  denied{}codemode resolve  ", " ".repeat(200));
        assert_eq!(error_line(&padded, false), "denied codemode resolve");
        assert_eq!(error_line(&padded, true), padded);
        assert_eq!(error_line(" \u{3000} ", false), "(the error text is empty)");
        // A long text is cut, never inside an escape, with its whole size.
        let long = format!("{}\u{1b}{}", "a".repeat(234), "b".repeat(50));
        assert_eq!(
            error_line(&long, false),
            format!("{} ... (285 bytes)", "a".repeat(234))
        );
        assert_eq!(
            error_line(&long, true),
            format!("{}\\u{{1b}}{}", "a".repeat(234), "b".repeat(50))
        );
        let fits = "x".repeat(ERROR_TEXT);
        assert_eq!(error_line(&fits, false), fits);
    }

    #[test]
    fn a_tool_with_one_write_lists_one_argument_per_line() {
        let long = "word ".repeat(60);
        let mut plan = many(3);
        plan.tools
            .insert("issues.create".into(), planned(false, false, true));
        plan.writes.push(
            serde_json::from_value(json!({"id": "ticket", "tool": "issues.create",
                "arguments": {"description": long, "team": "core", "title": "Triage"}}))
            .expect("write"),
        );
        let text = reviewed(&plan, false);
        assert!(
            text.contains(&format!(
                "issues.create  1 write  not repeatable\n  ticket\n    description  \"{} ...\" (300 bytes)\n    team         \"core\"\n    title        \"Triage\"\n",
                "word ".repeat(11).trim_end()
            )),
            "{text}"
        );
        let full = reviewed(&plan, true);
        assert!(
            full.contains(&format!(
                "  ticket\n    description  \"{long}\"\n    team         \"core\"\n"
            )),
            "{full}"
        );
    }

    #[test]
    fn a_plan_with_skipped_writes_is_finished() {
        let mut writes = vec![
            write("put-a", "notes.put", WriteState::Applied, NextAction::None),
            write("put-b", "notes.put", WriteState::Resolved, NextAction::None),
        ];
        for id in ["drop-c", "drop-d"] {
            writes.push(write(
                id,
                "notes.delete",
                WriteState::Skipped,
                NextAction::None,
            ));
        }
        let report = approved("done: 2 applied, 2 skipped", PlanState::Done, writes);
        assert_eq!(
            status(&report, &many(1), OPTIONS),
            format!(
                "\
triage.plan.json  sha256:{digest}
server   https://mcp.example.com/mcp
config   server.json  sha256:{config}
status   done: 2 applied, 2 skipped
         approved by alberto on 2026-11-11 12:00 UTC

applied 1 · failed 0 · no reply 0 · not sent 0 · resolved 1 · skipped 2

resolved by a person (1): recorded as applied
  put-b

skipped by a person (2): not applied, and never sent
  drop-c, drop-d

applied (1)
  put-a
",
                digest = Digest::of(b"fixed").short(),
                config = Digest::of(b"server file").short()
            )
        );
    }

    #[test]
    fn one_write_is_reviewed_in_full() {
        let plan = plan_of(
            &[("notes.put", planned(true, false, true))],
            vec![json!({"id": "mark-a", "tool": "notes.put",
                        "arguments": {"id": "a", "text": "alpha (reviewed)"}})],
            Some("Mark every note as reviewed"),
        );
        assert_eq!(
            reviewed(&plan, false),
            format!(
                "\
triage.plan.json  sha256:{digest}
server   https://mcp.example.com/mcp  (MCP 2025-11-25)
config   server.json  sha256:{config}
summary  Mark every note as reviewed
status   not approved

1 write with 1 tool

notes.put  1 write  repeatable
  mark-a
    id    \"a\"
    text  \"alpha (reviewed)\"

Notes
  Tool classes come from the server's own hints.

If a reply is lost: a repeatable write is sent again (up to 3 attempts). Any other write waits for a person.
Sending: the first write alone, then up to 4 at a time. A failed write stops the rest.
Next: codemode apply triage.plan.json
",
                digest = Digest::of(b"fixed").short(),
                config = Digest::of(b"server file").short()
            )
        );
    }

    fn triage() -> PlanFile {
        let mut writes = Vec::new();
        for issue in [101, 102, 105, 106, 110, 111, 112, 113] {
            writes.push(
                json!({"id": format!("label-{issue}"), "tool": "GITHUB_ADD_LABELS_TO_AN_ISSUE",
                "arguments": {"owner": "acme", "repo": "app", "issue_number": issue,
                              "labels": ["needs-triage"]}}),
            );
        }
        writes.push(json!({"id": "comment-101", "tool": "GITHUB_CREATE_AN_ISSUE_COMMENT",
            "arguments": {"owner": "acme", "repo": "app", "issue_number": 101,
                          "body": "Thanks for the report. Can you add the version you are running and the full output?"}}));
        writes.push(
            json!({"id": "comment-105", "tool": "GITHUB_CREATE_AN_ISSUE_COMMENT",
            "arguments": {"owner": "acme", "repo": "app", "issue_number": 105,
                          "body": "This looks like a duplicate of #88."}}),
        );
        writes.push(json!({"id": "close-105", "tool": "GITHUB_DELETE_AN_ISSUE",
            "arguments": {"owner": "acme", "repo": "app", "issue_number": 105}}));
        writes.push(json!({"id": "ping", "tool": "INTERNAL_PING", "arguments": {"value": "x"}}));
        writes.push(
            json!({"id": "ping-again", "tool": "INTERNAL_PING", "arguments": {"value": "x"}}),
        );
        plan_of(
            &[
                ("GITHUB_ADD_LABELS_TO_AN_ISSUE", planned(true, false, true)),
                (
                    "GITHUB_CREATE_AN_ISSUE_COMMENT",
                    planned(false, false, true),
                ),
                ("GITHUB_DELETE_AN_ISSUE", planned(true, true, true)),
                ("INTERNAL_PING", planned(false, false, false)),
            ],
            writes,
            Some("Label untriaged issues"),
        )
    }

    #[test]
    fn a_batch_is_reviewed_by_tool_with_what_is_the_same_and_what_differs() {
        assert_eq!(
            reviewed(&triage(), false),
            format!(
                "\
triage.plan.json  sha256:{digest}
server   https://mcp.example.com/mcp  (MCP 2025-11-25)
config   server.json  sha256:{config}
summary  Label untriaged issues
status   not approved

13 writes with 4 tools

GITHUB_DELETE_AN_ISSUE  1 write  repeatable  destructive
  close-105
    issue_number  105
    owner         \"acme\"
    repo          \"app\"

INTERNAL_PING  2 writes  not repeatable  no annotations
  same in every write   value=\"x\"
  different             nothing: every write has the same arguments

  id
  ping
  ping-again

GITHUB_ADD_LABELS_TO_AN_ISSUE  8 writes  repeatable
  same in every write   labels=[\"needs-triage\"]  owner=\"acme\"  repo=\"app\"
  different             issue_number: 8 values (101, 102, 105, 106, ...)

  id         issue_number
  label-101  101
  label-102  102
  label-105  105
  label-106  106
  label-110  110
  label-111  111
  label-112  112
  label-113  113

GITHUB_CREATE_AN_ISSUE_COMMENT  2 writes  not repeatable
  same in every write   owner=\"acme\"  repo=\"app\"
  different             body: 2 values
                        issue_number: 2 values (101, 105)

  id           body                                                                     issue_number
  comment-101  \"Thanks for the report. Can you add the version you are ...\" (83 bytes)  101
  comment-105  \"This looks like a duplicate of #88.\"                                    105

Notes
  ! 1 tool has no annotations from the server: INTERNAL_PING. Its writes are not sent again after a lost reply.
  ! 1 write uses a tool the server marks destructive: GITHUB_DELETE_AN_ISSUE.
  ! 2 writes are identical to another write (same tool, same arguments):
    ping, ping-again
  Tool classes come from the server's own hints.

If a reply is lost: a repeatable write is sent again (up to 3 attempts). Any other write waits for a person.
Sending: the first write alone, then up to 4 at a time. A failed write stops the rest.
Next: codemode apply triage.plan.json
",
                digest = Digest::of(b"fixed").short(),
                config = Digest::of(b"server file").short()
            )
        );
    }

    #[test]
    fn full_prints_every_write_as_canonical_json() {
        let text = reviewed(&triage(), true);
        assert!(text.contains(
            "  comment-101\n    {\"body\":\"Thanks for the report. Can you add the version you are running and the full output?\",\"issue_number\":101,\"owner\":\"acme\",\"repo\":\"app\"}\n"
        ), "{text}");
        // A tool with one write lists its arguments one per line.
        assert!(
            text.contains(
                "  close-105\n    issue_number  105\n    owner         \"acme\"\n    repo          \"app\"\n"
            ),
            "{text}"
        );
        assert!(
            !text.contains("--full") && !text.contains("bytes)"),
            "{text}"
        );
    }

    fn many(count: usize) -> PlanFile {
        let writes = (0..count)
            .map(|index| {
                json!({"id": format!("put-{index}"), "tool": "notes.put",
                       "arguments": {"id": format!("note-{index}"), "text": "reviewed"}})
            })
            .collect();
        plan_of(&[("notes.put", planned(true, false, true))], writes, None)
    }

    #[test]
    fn five_thousand_writes_fit_on_a_screen_and_say_what_is_hidden() {
        let plan = many(5000);
        let text = reviewed(&plan, false);
        assert!(text.lines().count() < 45, "{text}");
        assert!(text.contains("5,000 writes with 1 tool\n"), "{text}");
        assert!(text.contains(
            "  same in every write   text=\"reviewed\"\n  different             id: 5,000 values (\"note-0\", \"note-1\", \"note-2\", \"note-3\", ...)\n"
        ), "{text}");
        assert!(
            text.contains("  put-19  \"note-19\"\n  ... 4,980 more (--full)\n"),
            "{text}"
        );
        assert!(!text.contains("put-20 "), "{text}");
        let full = reviewed(&plan, true);
        assert!(full.contains("  put-4999\n    {\"id\":\"note-4999\",\"text\":\"reviewed\"}\n"));
        assert_eq!(full.matches("\n    {\"id\":").count(), 5000);
    }

    #[test]
    fn hostile_text_never_reaches_the_terminal_or_starts_a_line() {
        let hostile = "x\u{1b}[2J\u{1b}[HAPPROVED\nforged line \u{202e}desrever";
        let mut plan = plan_of(
            &[("notes.put\u{202e}", planned(false, false, true))],
            vec![
                json!({"id": "a", "tool": "notes.put\u{202e}", "arguments": {"id": hostile, hostile: 1}}),
                json!({"id": "b", "tool": "notes.put\u{202e}", "arguments": {"id": "other", hostile: 2}}),
                json!({"id": "c", "tool": "notes.put\u{202e}", "arguments": {"id": "third", hostile: 2}}),
            ],
            Some(hostile),
        );
        plan.server.identity = "https://mcp.example.com/\u{202e}pcm".into();
        for full in [false, true] {
            let text = reviewed(&plan, full);
            assert!(
                text.chars().all(|c| c == '\n' || !unsafe_to_print(c)),
                "{text:?}"
            );
            assert!(!text.contains("\nforged line"), "{text}");
            assert!(
                text.contains(if full {
                    r#"summary  "x\u001b[2J\u001b[HAPPROVED\nforged line \u202edesrever""#
                } else {
                    r"summary  x\u{1b}[2J\u{1b}[HAPPROVED forged line \u{202e}desrever"
                }),
                "{text}"
            );
            assert!(text.contains(r"server   https://mcp.example.com/\u{202e}pcm  (MCP"));
            assert!(text.contains(r"notes.put\u{202e}  3 writes"), "{text}");
            assert!(
                text.contains(r#""x\u001b[2J\u001b[HAPPROVED\nforged line \u202edesrever""#),
                "{text}"
            );
        }
        let asked = question(&plan);
        assert!(asked.chars().all(|c| !unsafe_to_print(c)), "{asked:?}");
    }

    #[test]
    fn the_question_names_the_writes_the_server_and_what_is_risky() {
        assert_eq!(
            question(&many(3)),
            "Apply these 3 writes through https://mcp.example.com/mcp? Type \"yes\" to approve: "
        );
        assert_eq!(
            question(&many(1)),
            "Apply this 1 write through https://mcp.example.com/mcp? Type \"yes\" to approve: "
        );
        assert_eq!(
            question(&triage()),
            "1 write uses a tool the server marks destructive.\n\
             2 writes use a tool without annotations from the server.\n\
             Apply these 13 writes through https://mcp.example.com/mcp? Type \"yes\" to approve: "
        );
    }

    fn write(id: &str, tool: &str, state: WriteState, next: NextAction) -> WriteStatus {
        WriteStatus {
            id: id.into(),
            tool: tool.into(),
            state,
            attempts: u32::from(state != WriteState::NotSent),
            error_code: None,
            error_text: None,
            answered: matches!(state, WriteState::Applied | WriteState::Failed),
            next,
            resolved_by: None,
        }
    }

    fn approved(detail: &str, state: PlanState, writes: Vec<WriteStatus>) -> StatusReport {
        let mut report = StatusReport {
            file: "triage.plan.json".into(),
            digest: Digest::of(b"fixed"),
            server: ServerStatus {
                identity: "https://mcp.example.com/mcp".into(),
                protocol: "2025-11-25".into(),
                config: Digest::of(b"server file"),
                file: Some("server.json".into()),
                file_state: ServerFileState::Same,
                command: None,
            },
            summary: Some("Label untriaged issues".into()),
            state,
            detail: detail.into(),
            approval: Some(ApprovalStatus {
                approver: "alberto".into(),
                approved_unix_ms: 1_794_398_400_000,
                approved_at: "2026-11-11 12:00 UTC".into(),
                concurrency: 4,
                max_attempts: 3,
                digest: Some(Digest::of(b"fixed")),
            }),
            counts: Counts::default(),
            writes,
        };
        let count = |state: WriteState| report.writes.iter().filter(|w| w.state == state).count();
        report.counts = Counts {
            total: report.writes.len(),
            applied: count(WriteState::Applied),
            failed: count(WriteState::Failed),
            no_reply: count(WriteState::NoReply),
            refused: count(WriteState::Refused),
            resolved: count(WriteState::Resolved),
            skipped: count(WriteState::Skipped),
            not_sent: count(WriteState::NotSent),
        };
        report
    }

    #[test]
    fn a_stopped_plan_says_what_happened_and_the_exact_next_command() {
        let mut writes = vec![
            write("label-101", "LABEL", WriteState::Applied, NextAction::None),
            write("label-102", "LABEL", WriteState::Applied, NextAction::None),
            write("label-105", "LABEL", WriteState::Applied, NextAction::None),
        ];
        let mut failed = write(
            "comment-101",
            "GITHUB_CREATE_AN_ISSUE_COMMENT",
            WriteState::Failed,
            NextAction::Resolve,
        );
        failed.error_code = Some("tool_error".into());
        failed.error_text = Some("Resource not accessible by integration".into());
        writes.push(failed);
        for index in 0..210 {
            writes.push(write(
                &format!("label-{}", 200 + index),
                "LABEL",
                WriteState::NotSent,
                NextAction::Apply,
            ));
        }
        let report = approved("stopped after 1 failure", PlanState::NeedsAttention, writes);
        assert_eq!(
            status(&report, &many(1), OPTIONS),
            format!(
                "\
triage.plan.json  sha256:{digest}
server   https://mcp.example.com/mcp
config   server.json  sha256:{config}
status   stopped after 1 failure
         approved by alberto on 2026-11-11 12:00 UTC

applied 3 · failed 1 · no reply 0 · not sent 210

failed (1): the server answered with an error
  comment-101  GITHUB_CREATE_AN_ISSUE_COMMENT
    Resource not accessible by integration
    codemode cannot tell whether a failed write changed anything. Check the target, then run:
      codemode resolve triage.plan.json comment-101 --as not-applied   (or --as applied)
    Use --as skipped for a write that must not be sent.

not sent (210)
  Run `codemode apply triage.plan.json` to continue.

applied (3)
  label-101, label-102, label-105
",
                digest = Digest::of(b"fixed").short(),
                config = Digest::of(b"server file").short()
            )
        );
    }

    #[test]
    fn every_section_appears_in_order_with_grouped_errors_and_bounded_lists() {
        let mut writes = Vec::new();
        for index in 0..30 {
            writes.push(write(
                &format!("ok-{index}"),
                "notes.put",
                WriteState::Applied,
                NextAction::None,
            ));
        }
        for index in 0..3 {
            let mut failed = write(
                &format!("append-{index}"),
                "notes.append",
                WriteState::Failed,
                NextAction::Resolve,
            );
            failed.error_text = Some("upstream said no\u{1b}[31m \u{202e}".into());
            writes.push(failed);
        }
        let mut other = write("put-x", "notes.put", WriteState::Failed, NextAction::Apply);
        other.error_text = Some("the server returned error -32603: internal error".into());
        writes.push(other);
        let mut silent = write(
            "append-9",
            "notes.append",
            WriteState::NoReply,
            NextAction::Resolve,
        );
        silent.error_code = Some("deadline".into());
        writes.push(silent);
        let mut refused = write(
            "put-y",
            "notes.put",
            WriteState::Refused,
            NextAction::Resolve,
        );
        refused.error_text = Some("the server returned HTTP status 403 Forbidden".into());
        writes.push(refused);
        let mut spent = write(
            "put-z",
            "notes.put",
            WriteState::Refused,
            NextAction::NewPlan,
        );
        spent.error_text = Some("the server returned HTTP status 429 Too Many Requests".into());
        writes.push(spent);
        writes.push(write(
            "append-7",
            "notes.append",
            WriteState::Resolved,
            NextAction::None,
        ));
        for id in ["drop-1", "drop-2"] {
            writes.push(write(
                id,
                "notes.delete",
                WriteState::Skipped,
                NextAction::None,
            ));
        }
        writes.push(write(
            "put-w",
            "notes.put",
            WriteState::NotSent,
            NextAction::Apply,
        ));
        let report = approved(
            "finished with 7 failures",
            PlanState::NeedsAttention,
            writes,
        );
        let text = status(&report, &many(1), OPTIONS);
        assert_eq!(
            text,
            format!(
                "\
triage.plan.json  sha256:{digest}
server   https://mcp.example.com/mcp
config   server.json  sha256:{config}
status   finished with 7 failures
         approved by alberto on 2026-11-11 12:00 UTC

applied 30 · failed 4 · no reply 1 · not sent 1 · refused 2 · resolved 1 · skipped 2

failed (4): the server answered with an error
  3 writes  notes.append
    upstream said no\\u{{1b}}[31m \\u{{202e}}
    append-0, append-1, append-2
  put-x  notes.put
    the server returned error -32603: internal error

  1 write is repeatable: `codemode apply triage.plan.json` sends it again (up to 3 attempts each).
  codemode cannot tell whether a failed write changed anything. Check the target, then run:
    codemode resolve triage.plan.json --errors --as not-applied   (or --as applied)
  Use --as skipped for a write that must not be sent.

no reply (1): the write may have reached the server, and no answer came back
  append-9  notes.append
    the deadline passed before an answer came back
    The write may or may not have happened. Check the target, then run:
      codemode resolve triage.plan.json append-9 --as not-applied   (or --as applied)
    Use --as skipped for a write that must not be sent.

refused (2): not applied, and not sent again
  put-y  notes.put
    the server returned HTTP status 403 Forbidden
  put-z  notes.put
    the server returned HTTP status 429 Too Many Requests

  The server refused 1 write before running it. Fix the cause, then run:
    codemode resolve triage.plan.json put-y --as not-applied   (apply then sends it again)
  Every attempt of 1 write is used. Make a new plan for it with `codemode plan`.
  Use --as skipped for a write that must not be sent.

resolved by a person (1): recorded as applied
  append-7

skipped by a person (2): not applied, and never sent
  drop-1, drop-2

not sent (1)
  Run `codemode apply triage.plan.json` to continue.

applied (30)
  ok-0, ok-1, ok-2, ok-3, ok-4, ok-5, ok-6, ok-7, ok-8, ok-9, ok-10, ok-11, ok-12, ok-13, ok-14,
  ok-15, ok-16, ok-17, ok-18, ok-19
  ... 10 more (--full)
",
                digest = Digest::of(b"fixed").short(),
                config = Digest::of(b"server file").short()
            )
        );
        // The error text's terminal sequence and U+202E are visible escapes.
        assert!(
            text.chars().all(|c| c == '\n' || !unsafe_to_print(c)),
            "{text:?}"
        );
        for word in [
            "probe",
            "reconcile",
            "contract",
            "binding",
            "scope",
            "provider",
        ] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        let full = status(
            &report,
            &many(1),
            ReviewOptions {
                full: true,
                ..OPTIONS
            },
        );
        assert!(
            full.contains("ok-29\n") && !full.contains("--full"),
            "{full}"
        );
    }

    #[test]
    fn many_different_errors_are_bounded_and_counted() {
        let writes: Vec<WriteStatus> = (0..50)
            .map(|index| {
                let mut failed = write(
                    &format!("w-{index}"),
                    "notes.append",
                    WriteState::Failed,
                    NextAction::Resolve,
                );
                failed.error_text = Some(format!("note {index} is locked"));
                failed
            })
            .collect();
        let report = approved(
            "finished with 50 failures",
            PlanState::NeedsAttention,
            writes,
        );
        let text = status(&report, &many(1), OPTIONS);
        assert!(
            text.contains("  w-19  notes.append\n    note 19 is locked\n  ... 30 more writes with 30 other error texts (--full)\n"),
            "{text}"
        );
        assert!(!text.contains("w-20 "), "{text}");
        assert!(text.contains("codemode resolve triage.plan.json --errors --as not-applied"));
        let full = status(
            &report,
            &many(1),
            ReviewOptions {
                full: true,
                ..OPTIONS
            },
        );
        assert!(full.contains("\n    note 49 is locked\n") && !full.contains("--full"));
    }

    #[test]
    fn an_edited_plan_shows_no_writes() {
        let mut report = approved("edited after approval", PlanState::Edited, Vec::new());
        report.digest = Digest::of(b"edited");
        let text = status(&report, &many(1), OPTIONS);
        assert!(text.contains("status   edited after approval\n"), "{text}");
        assert!(text.contains(&format!(
            "This file is not the plan that was approved: the approved plan is sha256:{}.",
            Digest::of(b"fixed").short()
        )));
        assert!(!text.contains("applied 0"), "{text}");
    }

    #[test]
    fn the_tool_list_says_what_a_plan_may_hold() {
        let tool = |name: &str, annotations: Value| {
            Tool::parse(&json!({"name": name, "inputSchema": {"type": "object"},
                                "annotations": annotations}))
            .expect("tool")
        };
        let listed = [
            (
                tool("notes.list", json!({"readOnlyHint": true})),
                ToolClass::Read,
            ),
            (
                tool(
                    "notes.put",
                    json!({"idempotentHint": true, "destructiveHint": false}),
                ),
                ToolClass::RepeatableWrite,
            ),
            (
                tool("notes.delete", json!({"destructiveHint": true})),
                ToolClass::Write,
            ),
            (tool("plain", Value::Null), ToolClass::Write),
        ];
        let mut facts = ServerFacts {
            identity: "https://mcp.example.com/mcp".into(),
            protocol: "2026-07-28".into(),
            trust_annotations: true,
            write_tools: Some(vec!["notes.put".into()]),
            tools: listed
                .into_iter()
                .map(|(tool, class)| (tool.name.clone(), (tool, class)))
                .collect(),
        };
        assert_eq!(
            tools(&facts),
            "\
server   https://mcp.example.com/mcp  (MCP 2026-07-28)

  tool          class             server marks it  in a plan
  notes.delete  write             destructive      no: not in write_tools
  notes.list    read              -                no: a program may call it
  notes.put     repeatable write  -                yes
  plain         write             no annotations   no: not in write_tools

write_tools allows 1 of 3 write tools in a plan.
"
        );
        facts.trust_annotations = false;
        facts.write_tools = None;
        for entry in facts.tools.values_mut() {
            entry.1 = ToolClass::Write;
        }
        let text = tools(&facts);
        assert!(text.ends_with(
            "\ntrust_annotations is off: every tool is a write; set trust_annotations or read_tools to let a program read.\n"
        ), "{text}");
        assert!(!text.contains("write_tools"), "{text}");
    }

    #[test]
    fn a_resolution_lists_what_it_selected_by_error_text() {
        let mut first = write(
            "append-0",
            "notes.append",
            WriteState::Failed,
            NextAction::Resolve,
        );
        first.error_text = Some("upstream said no".into());
        let mut second = write(
            "append-1",
            "notes.append",
            WriteState::Failed,
            NextAction::Resolve,
        );
        second.error_text = Some("upstream said no".into());
        let mut third = write(
            "put-3",
            "notes.put",
            WriteState::NoReply,
            NextAction::Resolve,
        );
        third.error_code = Some("no_result".into());
        assert_eq!(
            resolution(
                &[&first, &second, &third],
                Resolution::NotApplied,
                "alberto"
            ),
            "\
Recording 3 writes as not applied (reviewer alberto):

  2 writes  notes.append
    upstream said no
    append-0, append-1
  put-3  notes.put
    no answer is recorded: codemode is still waiting for it, or stopped before it came
"
        );
    }
}
