//! Command-line parsing and command dispatch (`SPEC.md`, "Commands").

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::{Git, WorktreeInfo};
use crate::index::{BaseIndex, LineRef, ensure_index, resolve_line};
use crate::model::FactKind;
use crate::output::{
    Report, ReportStatus, Stamp, WorktreeStamp, render_error, render_json, render_text,
};
use crate::overlay::{
    Overlay, View, active_worktrees, base_view, find_worktree, overlay_for, worktree_view,
};
use crate::store::Store;

const GENERAL_USAGE: &str = concat!(
    "arlineo <command> [options] [arguments]\n",
    "\n",
    "Commands:\n",
    "  status                              base, store and worktree state\n",
    "  index [REV]                         build or update the index of REV (default: the line)\n",
    "  find [--kind K] [--where F=GLOB]... facts matching conditions\n",
    "  table NAME                          one table\n",
    "  route PATH                          routes and the calls that reach them\n",
    "  fn NAME                             one function: callers, callees, facts inside\n",
    "  graph FROM [--up N] [--down N]      walk the graph (defaults: --up 1 --down 1)\n",
    "  impact TARGET [--depth N]           what a change can reach (default depth 5)\n",
    "  conflicts                           collisions between active worktrees\n",
    "  check [--worktree W]                new rule violations against the line\n",
    "  diff BRANCH...                      what branches change and where they collide\n",
    "  help [COMMAND]                      usage\n",
    "\n",
    "Common options:\n",
    "  --worktree W   answer from the view of worktree W (path, directory name or branch)\n",
    "  --base REF     use REF as the line instead of the configured one\n",
    "  --json         JSON output\n",
    "  --limit N      at most N results (default 200)\n",
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Status,
    Index,
    Find,
    Table,
    Route,
    Fn,
    Graph,
    Impact,
    Conflicts,
    Check,
    Diff,
    Help,
}

#[derive(Clone, Debug)]
struct Parsed {
    command: Command,
    positionals: Vec<String>,
    worktree: Option<String>,
    base: Option<String>,
    json: bool,
    limit: usize,
    kind: Option<FactKind>,
    wheres: Vec<(String, String)>,
    up: u32,
    down: u32,
    depth: u32,
}

#[derive(Clone, Debug)]
struct ParseError {
    message: String,
    command: Option<Command>,
    json: bool,
}

enum ParseOutcome {
    Help(Option<Command>),
    Run(Box<Parsed>),
}

fn command_from_name(name: &str) -> Option<Command> {
    match name {
        "status" => Some(Command::Status),
        "index" => Some(Command::Index),
        "find" => Some(Command::Find),
        "table" => Some(Command::Table),
        "route" => Some(Command::Route),
        "fn" => Some(Command::Fn),
        "graph" => Some(Command::Graph),
        "impact" => Some(Command::Impact),
        "conflicts" => Some(Command::Conflicts),
        "check" => Some(Command::Check),
        "diff" => Some(Command::Diff),
        "help" => Some(Command::Help),
        _ => None,
    }
}

fn command_name(command: Command) -> &'static str {
    match command {
        Command::Status => "status",
        Command::Index => "index",
        Command::Find => "find",
        Command::Table => "table",
        Command::Route => "route",
        Command::Fn => "fn",
        Command::Graph => "graph",
        Command::Impact => "impact",
        Command::Conflicts => "conflicts",
        Command::Check => "check",
        Command::Diff => "diff",
        Command::Help => "help",
    }
}

fn usage_line(command: Command) -> &'static str {
    match command {
        Command::Status => "usage: arlineo status [--worktree W] [--base REF] [--json] [--limit N]",
        Command::Index => {
            "usage: arlineo index [REV] [--worktree W] [--base REF] [--json] [--limit N]"
        }
        Command::Find => {
            "usage: arlineo find [--kind K] [--where F=GLOB]... [--limit N] [--worktree W] [--base REF] [--json]"
        }
        Command::Table => {
            "usage: arlineo table NAME [--worktree W] [--base REF] [--json] [--limit N]"
        }
        Command::Route => {
            "usage: arlineo route PATH [--worktree W] [--base REF] [--json] [--limit N]"
        }
        Command::Fn => "usage: arlineo fn NAME [--limit N] [--worktree W] [--base REF] [--json]",
        Command::Graph => {
            "usage: arlineo graph FROM [--up N] [--down N] [--worktree W] [--base REF] [--json]"
        }
        Command::Impact => {
            "usage: arlineo impact TARGET [--depth N] [--worktree W] [--base REF] [--json]"
        }
        Command::Conflicts => {
            "usage: arlineo conflicts [--worktree W] [--base REF] [--json] [--limit N]"
        }
        Command::Check => "usage: arlineo check [--worktree W] [--base REF] [--json] [--limit N]",
        Command::Diff => {
            "usage: arlineo diff BRANCH... [--worktree W] [--base REF] [--json] [--limit N]"
        }
        Command::Help => "usage: arlineo help [COMMAND]",
    }
}

fn usage_error(message: String, command: Option<Command>, json: bool) -> ParseError {
    ParseError {
        message,
        command,
        json,
    }
}

fn option_allowed(command: Command, name: &str) -> bool {
    // --worktree, --base, --json and --limit are the common options of SPEC 5;
    // the other options belong to one command each.
    match name {
        "json" | "base" | "worktree" | "limit" => true,
        "kind" | "where" => command == Command::Find,
        "up" | "down" => command == Command::Graph,
        "depth" => command == Command::Impact,
        _ => false,
    }
}

fn parse_positive_usize(
    value: &str,
    option: &str,
    command: Command,
    json: bool,
) -> std::result::Result<usize, ParseError> {
    match value.parse::<usize>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err(usage_error(
            format!("option --{option} needs a positive integer, got '{value}'"),
            Some(command),
            json,
        )),
    }
}

fn parse_u32_option(
    value: &str,
    option: &str,
    command: Command,
    json: bool,
) -> std::result::Result<u32, ParseError> {
    match value.parse::<u32>() {
        Ok(number) => Ok(number),
        Err(_) => Err(usage_error(
            format!("option --{option} needs a non-negative integer, got '{value}'"),
            Some(command),
            json,
        )),
    }
}

fn parse_where(
    value: &str,
    command: Command,
    json: bool,
) -> std::result::Result<(String, String), ParseError> {
    match value.split_once('=') {
        Some((field, glob)) if !field.is_empty() && !glob.is_empty() => {
            Ok((field.to_owned(), glob.to_owned()))
        }
        _ => Err(usage_error(
            format!("option --where needs F=GLOB with both sides non-empty, got '{value}'"),
            Some(command),
            json,
        )),
    }
}

fn parse(args: Vec<OsString>) -> std::result::Result<ParseOutcome, ParseError> {
    let mut strings: Vec<String> = Vec::new();
    for arg in args {
        match arg.into_string() {
            Ok(text) => strings.push(text),
            Err(_) => {
                return Err(usage_error(String::from("non-UTF-8 argument"), None, false));
            }
        }
    }
    let json_hint = strings.iter().any(|arg| arg == "--json");
    let first = match strings.first() {
        Some(text) => text.clone(),
        None => {
            return Err(usage_error(
                String::from("no command given"),
                None,
                json_hint,
            ));
        }
    };
    if first == "help" {
        if strings.len() == 1 {
            return Ok(ParseOutcome::Help(None));
        }
        if strings.len() == 2 {
            let target = match strings.get(1) {
                Some(text) => text.clone(),
                None => String::new(),
            };
            if target == "-h" || target == "--help" {
                return Ok(ParseOutcome::Help(None));
            }
            return match command_from_name(&target) {
                Some(command) => Ok(ParseOutcome::Help(Some(command))),
                None => Err(usage_error(
                    format!("unknown command '{target}'"),
                    Some(Command::Help),
                    json_hint,
                )),
            };
        }
        return Err(usage_error(
            String::from("help takes at most one argument"),
            Some(Command::Help),
            json_hint,
        ));
    }
    if first == "-h" || first == "--help" {
        if strings.len() == 1 {
            return Ok(ParseOutcome::Help(None));
        }
        return Err(usage_error(
            String::from("help takes no further arguments"),
            None,
            json_hint,
        ));
    }
    let command = match command_from_name(&first) {
        Some(command) => command,
        None => {
            return Err(usage_error(
                format!("unknown command '{first}'"),
                None,
                json_hint,
            ));
        }
    };
    let mut parsed = Parsed {
        command,
        positionals: Vec::new(),
        worktree: None,
        base: None,
        json: false,
        limit: 200,
        kind: None,
        wheres: Vec::new(),
        up: 1,
        down: 1,
        depth: 5,
    };
    let mut limit_seen = false;
    let mut kind_seen = false;
    let mut up_seen = false;
    let mut down_seen = false;
    let mut depth_seen = false;
    let mut options_ended = false;
    let mut index = 1usize;
    while let Some(arg) = strings.get(index) {
        let arg = arg.clone();
        if !options_ended && arg == "--" {
            options_ended = true;
            index = match index.checked_add(1) {
                Some(next) => next,
                None => {
                    return Err(usage_error(
                        String::from("too many arguments"),
                        Some(command),
                        parsed.json || json_hint,
                    ));
                }
            };
            continue;
        }
        if !options_ended && arg.starts_with("--") {
            let body = match arg.get(2..) {
                Some(body) => body.to_owned(),
                None => String::new(),
            };
            let (name, inline_value) = match body.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (body, None),
            };
            if !option_allowed(command, &name) {
                let message = if matches!(
                    name.as_str(),
                    "worktree"
                        | "base"
                        | "json"
                        | "limit"
                        | "kind"
                        | "where"
                        | "up"
                        | "down"
                        | "depth"
                ) {
                    format!(
                        "option --{name} is not valid for command {}",
                        command_name(command)
                    )
                } else {
                    format!("unknown option --{name}")
                };
                return Err(usage_error(
                    message,
                    Some(command),
                    parsed.json || json_hint,
                ));
            }
            if name == "json" {
                if inline_value.is_some() {
                    return Err(usage_error(
                        String::from("option --json takes no value"),
                        Some(command),
                        parsed.json || json_hint,
                    ));
                }
                if parsed.json {
                    return Err(usage_error(
                        String::from("option --json given more than once"),
                        Some(command),
                        true,
                    ));
                }
                parsed.json = true;
                index = match index.checked_add(1) {
                    Some(next) => next,
                    None => {
                        return Err(usage_error(
                            String::from("too many arguments"),
                            Some(command),
                            parsed.json,
                        ));
                    }
                };
                continue;
            }
            let value = match inline_value {
                Some(value) => {
                    if value.is_empty() {
                        return Err(usage_error(
                            format!("option --{name} requires a value"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    value
                }
                None => {
                    let next_index = match index.checked_add(1) {
                        Some(next) => next,
                        None => {
                            return Err(usage_error(
                                format!("option --{name} requires a value"),
                                Some(command),
                                parsed.json || json_hint,
                            ));
                        }
                    };
                    match strings.get(next_index) {
                        Some(value) if !value.starts_with("--") => {
                            index = next_index;
                            value.clone()
                        }
                        _ => {
                            return Err(usage_error(
                                format!("option --{name} requires a value"),
                                Some(command),
                                parsed.json || json_hint,
                            ));
                        }
                    }
                }
            };
            match name.as_str() {
                "worktree" => {
                    if parsed.worktree.is_some() {
                        return Err(usage_error(
                            String::from("option --worktree given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    parsed.worktree = Some(value);
                }
                "base" => {
                    if parsed.base.is_some() {
                        return Err(usage_error(
                            String::from("option --base given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    parsed.base = Some(value);
                }
                "limit" => {
                    if limit_seen {
                        return Err(usage_error(
                            String::from("option --limit given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    limit_seen = true;
                    parsed.limit =
                        parse_positive_usize(&value, "limit", command, parsed.json || json_hint)?;
                }
                "kind" => {
                    if kind_seen {
                        return Err(usage_error(
                            String::from("option --kind given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    kind_seen = true;
                    parsed.kind = match FactKind::parse(&value) {
                        Some(kind) => Some(kind),
                        None => {
                            return Err(usage_error(
                                format!("invalid kind '{value}'"),
                                Some(command),
                                parsed.json || json_hint,
                            ));
                        }
                    };
                }
                "where" => {
                    let condition = parse_where(&value, command, parsed.json || json_hint)?;
                    parsed.wheres.push(condition);
                }
                "up" => {
                    if up_seen {
                        return Err(usage_error(
                            String::from("option --up given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    up_seen = true;
                    parsed.up = parse_u32_option(&value, "up", command, parsed.json || json_hint)?;
                }
                "down" => {
                    if down_seen {
                        return Err(usage_error(
                            String::from("option --down given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    down_seen = true;
                    parsed.down =
                        parse_u32_option(&value, "down", command, parsed.json || json_hint)?;
                }
                "depth" => {
                    if depth_seen {
                        return Err(usage_error(
                            String::from("option --depth given more than once"),
                            Some(command),
                            parsed.json || json_hint,
                        ));
                    }
                    depth_seen = true;
                    parsed.depth =
                        parse_u32_option(&value, "depth", command, parsed.json || json_hint)?;
                }
                _ => {
                    return Err(usage_error(
                        format!("unknown option --{name}"),
                        Some(command),
                        parsed.json || json_hint,
                    ));
                }
            }
            index = match index.checked_add(1) {
                Some(next) => next,
                None => {
                    return Err(usage_error(
                        String::from("too many arguments"),
                        Some(command),
                        parsed.json || json_hint,
                    ));
                }
            };
            continue;
        }
        if !options_ended && arg.len() > 1 && arg.starts_with('-') {
            return Err(usage_error(
                format!("unknown option '{arg}'"),
                Some(command),
                parsed.json || json_hint,
            ));
        }
        parsed.positionals.push(arg);
        index = match index.checked_add(1) {
            Some(next) => next,
            None => {
                return Err(usage_error(
                    String::from("too many arguments"),
                    Some(command),
                    parsed.json || json_hint,
                ));
            }
        };
    }
    let count_ok = match command {
        Command::Index | Command::Help => parsed.positionals.len() <= 1,
        Command::Table | Command::Route | Command::Fn | Command::Graph | Command::Impact => {
            parsed.positionals.len() == 1
        }
        Command::Diff => !parsed.positionals.is_empty(),
        Command::Status | Command::Find | Command::Conflicts | Command::Check => {
            parsed.positionals.is_empty()
        }
    };
    if !count_ok {
        let message = match command {
            Command::Index => String::from("command index takes at most one argument"),
            Command::Table => String::from("command table takes exactly one argument"),
            Command::Route => String::from("command route takes exactly one argument"),
            Command::Fn => String::from("command fn takes exactly one argument"),
            Command::Graph => String::from("command graph takes exactly one argument"),
            Command::Impact => String::from("command impact takes exactly one argument"),
            Command::Diff => String::from("command diff takes at least one argument"),
            Command::Help => String::from("command help takes at most one argument"),
            Command::Status => String::from("command status takes no arguments"),
            Command::Find => String::from("command find takes no arguments"),
            Command::Conflicts => String::from("command conflicts takes no arguments"),
            Command::Check => String::from("command check takes no arguments"),
        };
        return Err(usage_error(message, Some(command), parsed.json));
    }
    Ok(ParseOutcome::Run(Box::new(parsed)))
}

fn write_stdout(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

fn write_stderr(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stderr().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

fn emit_error(error: &Error, json: bool, command: Option<Command>) -> i32 {
    let mut text = render_error(error, json);
    if !json && matches!(error, Error::Usage { .. }) {
        match command {
            Some(command) => {
                text.push_str(usage_line(command));
                text.push('\n');
            }
            None => {
                text.push_str("usage: arlineo <command> [options] [arguments]\n");
            }
        }
    }
    if json {
        match write_stdout(&text) {
            Ok(()) | Err(_) => error.exit_code(),
        }
    } else {
        match write_stderr(&text) {
            Ok(()) | Err(_) => error.exit_code(),
        }
    }
}

fn make_stamp(
    line: &LineRef,
    base: &BaseIndex,
    now_unix: u64,
    worktree: Option<WorktreeStamp>,
) -> Stamp {
    Stamp {
        line_name: line.name.clone(),
        line_commit: line.commit.clone(),
        indexed_unix: base.indexed_unix,
        now_unix,
        worktree,
    }
}

fn age_text(now_unix: u64, indexed_unix: u64) -> String {
    let age = now_unix.saturating_sub(indexed_unix);
    if age < 60 {
        format!("{age}s")
    } else if age < 3_600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3_600)
    } else {
        format!("{}d", age / 86_400)
    }
}

fn json_usize(value: usize) -> serde_json::Value {
    // usize is at most 64 bits on the targets Rust supports here, so this
    // conversion to u64 cannot lose a count.
    let number = value as u64;
    serde_json::Value::Number(serde_json::Number::from(number))
}

fn json_u64(value: u64) -> serde_json::Value {
    serde_json::Value::Number(serde_json::Number::from(value))
}

fn worktree_name(worktree: &WorktreeInfo) -> String {
    if let Some(branch) = &worktree.branch {
        return branch.clone();
    }
    match worktree.path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => worktree.path.display().to_string(),
    }
}

fn worktree_stamp(overlay: &Overlay) -> WorktreeStamp {
    WorktreeStamp {
        name: worktree_name(&overlay.worktree),
        merge_base: overlay.merge_base.clone(),
        changed_files: overlay.changed.len(),
    }
}

fn changed_text(count: usize) -> String {
    if count == 1 {
        String::from("1 changed file")
    } else {
        format!("{count} changed files")
    }
}

fn fact_counts(index: &BaseIndex) -> (Vec<(FactKind, usize)>, usize, usize) {
    let mut counts: Vec<(FactKind, usize)> = Vec::new();
    let mut total = 0usize;
    let mut unparsed = 0usize;
    for (kind, _name) in FactKind::ALL {
        let count = index.fact_index.of_kind(&index.facts, kind).count();
        total = total.saturating_add(count);
        if kind == FactKind::Unparsed {
            unparsed = count;
        }
        counts.push((kind, count));
    }
    (counts, total, unparsed)
}

fn fact_counts_json(counts: &[(FactKind, usize)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (kind, count) in counts {
        map.insert(String::from(kind.as_str()), json_usize(*count));
    }
    serde_json::Value::Object(map)
}

fn push_fact_counts(lines: &mut Vec<String>, counts: &[(FactKind, usize)]) {
    for (kind, count) in counts {
        lines.push(format!("facts {}: {count}", kind.as_str()));
    }
}

fn build_status(
    git: &Git,
    store: &Store,
    config: &Config,
    line: &LineRef,
    base: &BaseIndex,
    now_unix: u64,
) -> Result<Report> {
    let (counts, total, unparsed) = fact_counts(base);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("line: {}@{}", line.name, line.commit));
    lines.push(format!(
        "indexed: {} ({} ago)",
        base.indexed_unix,
        age_text(now_unix, base.indexed_unix)
    ));
    lines.push(format!("store: {}", store.root().display()));
    lines.push(format!("facts: {total}"));
    push_fact_counts(&mut lines, &counts);
    lines.push(format!("skipped files: {}", base.skipped));
    lines.push(format!("unparsed facts: {unparsed}"));
    let mut worktrees_json: Vec<serde_json::Value> = Vec::new();
    let active = active_worktrees(git)?;
    crate::overlay::prune_overlays(store, &active);
    for worktree in active {
        let name = worktree_name(&worktree);
        match overlay_for(git, store, config, line, &worktree) {
            Ok(overlay) => {
                lines.push(format!(
                    "worktree {}: merge-base {}, {}",
                    name,
                    overlay.merge_base,
                    changed_text(overlay.changed.len())
                ));
                let mut entry = serde_json::Map::new();
                entry.insert(String::from("name"), serde_json::Value::String(name));
                entry.insert(
                    String::from("merge_base"),
                    serde_json::Value::String(overlay.merge_base.as_str().to_owned()),
                );
                entry.insert(
                    String::from("changed_files"),
                    json_usize(overlay.changed.len()),
                );
                worktrees_json.push(serde_json::Value::Object(entry));
            }
            Err(error) => {
                let message = error.to_string();
                lines.push(format!("worktree {name}: error: {message}"));
                let mut entry = serde_json::Map::new();
                entry.insert(String::from("name"), serde_json::Value::String(name));
                entry.insert(String::from("error"), serde_json::Value::String(message));
                worktrees_json.push(serde_json::Value::Object(entry));
            }
        }
    }
    let mut json = serde_json::Map::new();
    json.insert(
        String::from("line"),
        serde_json::Value::String(line.name.clone()),
    );
    json.insert(
        String::from("commit"),
        serde_json::Value::String(line.commit.as_str().to_owned()),
    );
    json.insert(String::from("indexed_unix"), json_u64(base.indexed_unix));
    json.insert(
        String::from("index_age_seconds"),
        json_u64(now_unix.saturating_sub(base.indexed_unix)),
    );
    json.insert(
        String::from("store"),
        serde_json::Value::String(store.root().display().to_string()),
    );
    json.insert(String::from("facts_total"), json_usize(total));
    json.insert(String::from("facts"), fact_counts_json(&counts));
    json.insert(String::from("skipped"), json_usize(base.skipped));
    json.insert(String::from("skipped_files"), json_usize(base.skipped));
    json.insert(String::from("unparsed"), json_usize(unparsed));
    json.insert(String::from("unparsed_facts"), json_usize(unparsed));
    json.insert(
        String::from("worktrees"),
        serde_json::Value::Array(worktrees_json),
    );
    Ok(Report {
        command: String::from("status"),
        lines,
        json: serde_json::Value::Object(json),
        status: ReportStatus::Ok,
    })
}

fn build_index_report(index: &BaseIndex) -> Report {
    let (counts, total, _unparsed) = fact_counts(index);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("commit: {}", index.commit));
    lines.push(format!("indexed: {}", index.indexed_unix));
    lines.push(format!("facts: {total}"));
    push_fact_counts(&mut lines, &counts);
    lines.push(format!("skipped files: {}", index.skipped));
    let mut json = serde_json::Map::new();
    json.insert(
        String::from("commit"),
        serde_json::Value::String(index.commit.as_str().to_owned()),
    );
    json.insert(String::from("indexed_unix"), json_u64(index.indexed_unix));
    json.insert(String::from("facts_total"), json_usize(total));
    json.insert(String::from("facts"), fact_counts_json(&counts));
    json.insert(String::from("skipped"), json_usize(index.skipped));
    json.insert(String::from("skipped_files"), json_usize(index.skipped));
    Report {
        command: String::from("index"),
        lines,
        json: serde_json::Value::Object(json),
        status: ReportStatus::Ok,
    }
}

fn first_positional(parsed: &Parsed) -> Result<&str> {
    match parsed.positionals.first() {
        Some(value) => Ok(value.as_str()),
        None => Err(Error::Internal {
            message: String::from("missing positional argument after parsing"),
        }),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match std::fs::canonicalize(left) {
        Ok(canonical) => canonical == right,
        Err(_) => false,
    }
}

fn view_from_base(
    parsed: &Parsed,
    git: &Git,
    store: &Store,
    config: &Config,
    line: &LineRef,
    base: BaseIndex,
) -> Result<(View, Option<WorktreeStamp>)> {
    match &parsed.worktree {
        Some(spec) => {
            let worktree = find_worktree(git, spec)?;
            let overlay = overlay_for(git, store, config, line, &worktree)?;
            let stamp = worktree_stamp(&overlay);
            let view = worktree_view(store, &base, &overlay)?;
            Ok((view, Some(stamp)))
        }
        None => Ok((base_view(base), None)),
    }
}

fn run_check(
    parsed: &Parsed,
    git: &Git,
    store: &Store,
    config: &Config,
    line: &LineRef,
    base: BaseIndex,
    repo_top: &Path,
) -> Result<(Report, Option<WorktreeStamp>)> {
    // `check` compares the worktree view with the line itself, not with the merge-base: a
    // violation the line got from other people's commits after the merge-base is present on
    // both sides and is not reported as this worktree's.
    if let Some(spec) = &parsed.worktree {
        let worktree = find_worktree(git, spec)?;
        let overlay = overlay_for(git, store, config, line, &worktree)?;
        let stamp = worktree_stamp(&overlay);
        let now_view = worktree_view(store, &base, &overlay)?;
        let before_view = base_view(base);
        let report = crate::check::run(config, &now_view, &before_view)?;
        return Ok((report, Some(stamp)));
    }
    let current = active_worktrees(git)?
        .into_iter()
        .find(|worktree| same_path(&worktree.path, repo_top));
    match current {
        None => {
            let view = base_view(base);
            let report = crate::check::run(config, &view, &view)?;
            Ok((report, None))
        }
        Some(worktree) => {
            let overlay = overlay_for(git, store, config, line, &worktree)?;
            let head_differs = match &overlay.worktree.head {
                Some(head) => head != &line.commit,
                None => true,
            };
            if !head_differs && overlay.changed.is_empty() {
                let view = base_view(base);
                let report = crate::check::run(config, &view, &view)?;
                return Ok((report, None));
            }
            let stamp = worktree_stamp(&overlay);
            let now_view = worktree_view(store, &base, &overlay)?;
            let before_view = base_view(base);
            let report = crate::check::run(config, &now_view, &before_view)?;
            Ok((report, Some(stamp)))
        }
    }
}

fn run_conflicts(git: &Git, store: &Store, config: &Config, line: &LineRef) -> Result<Report> {
    let mut overlays: Vec<Overlay> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for worktree in active_worktrees(git)? {
        let name = worktree_name(&worktree);
        match overlay_for(git, store, config, line, &worktree) {
            Ok(overlay) => {
                let head_is_line = match &overlay.worktree.head {
                    Some(head) => head == &line.commit,
                    None => false,
                };
                if !(head_is_line && overlay.changed.is_empty()) {
                    overlays.push(overlay);
                }
            }
            Err(error) => {
                notes.push(format!("note: worktree {name}: {error}"));
            }
        }
    }
    let mut report = crate::conflicts::run(&overlays)?;
    if !notes.is_empty() {
        if let serde_json::Value::Object(map) = &mut report.json {
            if !map.contains_key("notes") {
                let values = notes
                    .iter()
                    .map(|note| serde_json::Value::String(note.clone()))
                    .collect();
                map.insert(String::from("notes"), serde_json::Value::Array(values));
            }
        }
        report.lines.extend(notes);
    }
    Ok(report)
}

fn execute(parsed: &Parsed) -> Result<(Stamp, Report)> {
    let current_dir: PathBuf = match std::env::current_dir() {
        Ok(path) => path,
        Err(source) => {
            return Err(Error::io("determine current directory", source));
        }
    };
    let git = Git::new(&current_dir);
    let repo = git.repo_info()?;
    let repo_top = repo.top.clone();
    let store = Store::open(&repo)?;
    let config = Config::load(&repo_top)?;
    let line = resolve_line(&git, &config, parsed.base.as_deref())?;
    let base = ensure_index(&git, &store, &config, &line.commit)?;
    let now_unix = crate::store::now_unix();
    // Commands that do not answer from a view still name an explicitly requested
    // worktree in the stamp; view commands and check build their own below.
    let flag_stamp = match &parsed.worktree {
        Some(spec)
            if matches!(
                parsed.command,
                Command::Status | Command::Index | Command::Conflicts | Command::Diff
            ) =>
        {
            let worktree = find_worktree(&git, spec)?;
            let overlay = overlay_for(&git, &store, &config, &line, &worktree)?;
            Some(worktree_stamp(&overlay))
        }
        _ => None,
    };
    match parsed.command {
        Command::Status => {
            let report = build_status(&git, &store, &config, &line, &base, now_unix)?;
            Ok((make_stamp(&line, &base, now_unix, flag_stamp), report))
        }
        Command::Index => {
            let report = match parsed.positionals.first() {
                Some(rev) => {
                    let commit = git.rev_parse(rev)?;
                    let index = ensure_index(&git, &store, &config, &commit)?;
                    build_index_report(&index)
                }
                None => build_index_report(&base),
            };
            Ok((make_stamp(&line, &base, now_unix, flag_stamp), report))
        }
        Command::Conflicts => {
            let report = run_conflicts(&git, &store, &config, &line)?;
            Ok((make_stamp(&line, &base, now_unix, flag_stamp), report))
        }
        Command::Diff => {
            let report = crate::diff::run(&git, &store, &config, &line, &parsed.positionals)?;
            Ok((make_stamp(&line, &base, now_unix, flag_stamp), report))
        }
        Command::Check => {
            let indexed_unix = base.indexed_unix;
            let (report, worktree) =
                run_check(parsed, &git, &store, &config, &line, base, &repo_top)?;
            Ok((
                Stamp {
                    line_name: line.name.clone(),
                    line_commit: line.commit.clone(),
                    indexed_unix,
                    now_unix,
                    worktree,
                },
                report,
            ))
        }
        Command::Help => Err(Error::Internal {
            message: String::from("help command reached command execution"),
        }),
        Command::Find
        | Command::Table
        | Command::Route
        | Command::Fn
        | Command::Graph
        | Command::Impact => {
            let indexed_unix = base.indexed_unix;
            let (view, worktree) = view_from_base(parsed, &git, &store, &config, &line, base)?;
            let report = match parsed.command {
                Command::Find => {
                    crate::query::find::run(&view, parsed.kind, &parsed.wheres, parsed.limit)?
                }
                Command::Table => crate::query::table::run(&view, first_positional(parsed)?)?,
                Command::Route => crate::query::route::run(&view, first_positional(parsed)?)?,
                Command::Fn => {
                    crate::query::fnq::run(&view, first_positional(parsed)?, parsed.limit)?
                }
                Command::Graph => crate::query::walk::run(
                    &view,
                    first_positional(parsed)?,
                    parsed.up,
                    parsed.down,
                )?,
                Command::Impact => {
                    crate::query::impact::run(&view, first_positional(parsed)?, parsed.depth)?
                }
                _ => {
                    return Err(Error::Internal {
                        message: String::from("command does not use a view"),
                    });
                }
            };
            Ok((
                Stamp {
                    line_name: line.name.clone(),
                    line_commit: line.commit.clone(),
                    indexed_unix,
                    now_unix,
                    worktree,
                },
                report,
            ))
        }
    }
}

/// Runs one command and returns the process exit code: 0 success, 1 error, 2 usage error,
/// 3 `check` found new violations. Writes results to stdout and errors to stderr.
pub fn run(args: Vec<OsString>) -> i32 {
    if args.is_empty() {
        match write_stderr(GENERAL_USAGE) {
            Ok(()) | Err(_) => 2,
        }
    } else {
        match parse(args) {
            Ok(ParseOutcome::Help(target)) => {
                let text = match target {
                    Some(command) => format!("{}\n", usage_line(command)),
                    None => String::from(GENERAL_USAGE),
                };
                match write_stdout(&text) {
                    Ok(()) => 0,
                    Err(error) => {
                        if error.kind() == std::io::ErrorKind::BrokenPipe {
                            0
                        } else {
                            1
                        }
                    }
                }
            }
            Ok(ParseOutcome::Run(parsed)) => {
                let parsed = *parsed;
                match execute(&parsed) {
                    Ok((stamp, report)) => {
                        let rendered = if parsed.json {
                            match render_json(&stamp, &report) {
                                Ok(text) => text,
                                Err(error) => {
                                    return emit_error(&error, parsed.json, Some(parsed.command));
                                }
                            }
                        } else {
                            render_text(&stamp, &report)
                        };
                        let exit = match report.status {
                            ReportStatus::Ok => 0,
                            ReportStatus::Findings => 3,
                        };
                        match write_stdout(&rendered) {
                            Ok(()) => exit,
                            // A closed pipe (`arlineo check | head`) keeps the command's own
                            // result, so findings still give exit code 3.
                            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => exit,
                            Err(_) => 1,
                        }
                    }
                    Err(error) => emit_error(&error, parsed.json, Some(parsed.command)),
                }
            }
            Err(error) => {
                let usage = Error::Usage {
                    message: error.message,
                };
                emit_error(&usage, error.json, error.command)
            }
        }
    }
}
