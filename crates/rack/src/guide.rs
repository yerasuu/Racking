//! The agent guide and the per-command help that ship inside the binary, so
//! `rack guide`, `rack help COMMAND` and the MCP tool `racking_guide` always
//! describe the version that is running. `COMMANDS` is the one table behind all of them.

use std::fmt::Write as _;

use crate::release::NOTES;

/// `docs/agent-guide.md`, embedded so the text cannot drift from the version it ships with.
pub const GUIDE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/agent-guide.md"));

/// One `rack` command: its one-line summary, the MCP tool that does the same, and its full help.
pub struct Command {
    pub name: &'static str,
    pub summary: &'static str,
    pub mcp: Option<&'static str>,
    /// `docs/commands/<name>.md`, readable on GitHub as well.
    pub help: &'static str,
}

macro_rules! command {
    ($name:literal, $summary:literal, $mcp:expr) => {
        Command {
            name: $name,
            summary: $summary,
            mcp: $mcp,
            help: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/commands/",
                $name,
                ".md"
            )),
        }
    };
}

pub const COMMANDS: &[Command] = &[
    command!(
        "whoami",
        "show who you are and which agents you can send to",
        Some("racking_whoami")
    ),
    command!(
        "agents",
        "list every agent: online or not, version, capabilities, about line",
        Some("racking_agents")
    ),
    command!("about", "set the line other agents see about you", None),
    command!(
        "capabilities",
        "publish what this machine has (windows, nvidia, kb...)",
        None
    ),
    command!(
        "say",
        "send a message: conversation text, files allowed",
        Some("racking_say")
    ),
    command!(
        "notify",
        "send a notice: a heads-up of up to 1 KiB, never answered",
        Some("racking_notify")
    ),
    command!(
        "report",
        "send a report: findings to read later, up to 1 MiB",
        Some("racking_report")
    ),
    command!("ask", "send a question and wait for the reply", Some("racking_ask")),
    command!(
        "send",
        "send a task: work with states (accept, result); also comments and results",
        Some("racking_send")
    ),
    command!("inbox", "list the jobs sent to you", Some("racking_inbox")),
    command!(
        "wait",
        "block until new jobs arrive; keeps the cursor",
        Some("racking_wait")
    ),
    command!(
        "watch",
        "wait without end, one short line per new job (for a Monitor or a hook)",
        None
    ),
    command!("show", "show one job in full", Some("racking_show")),
    command!("accept", "accept a task sent to you", Some("racking_accept")),
    command!("reject", "reject a task with a reason", Some("racking_reject")),
    command!("result", "send the result of an accepted task", Some("racking_result")),
    command!("attach", "add files to a job you just sent", Some("racking_attach")),
    command!(
        "get-attachment",
        "download one attachment and verify its sha256",
        Some("racking_get_attachment")
    ),
    command!(
        "outbox",
        "list the jobs you sent and their states",
        Some("racking_outbox")
    ),
    command!("pull", "turn new tasks into entries on the local kb board", None),
    command!(
        "update",
        "install the newest stable rack the server publishes; --pre: test builds too",
        None
    ),
    command!(
        "publish",
        "admin agents: publish a rack binary for others to update to",
        None
    ),
    command!(
        "guide",
        "how to use Racking, in general or for one command",
        Some("racking_guide")
    ),
    command!("version", "print the version", None),
    command!("help", "print the usage text, or the help of one command", None),
];

/// What `rack guide` can print.
pub enum Topic<'a> {
    Overview,
    Changes,
    List,
    Command(&'a str),
}

pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// The text for `rack guide`, `rack help COMMAND` and `racking_guide`. An unknown command is
/// an error that names the valid ones, so the caller can show it as a usage error.
pub fn render(topic: &Topic) -> Result<String, String> {
    match topic {
        Topic::Overview => Ok(overview()),
        Topic::Changes => Ok(changes()),
        Topic::List => Ok(names()),
        Topic::Command(name) => find(name).map(|c| c.help.to_owned()).ok_or_else(|| {
            format!(
                "unknown command `{name}`; valid commands: {}",
                COMMANDS.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
            )
        }),
    }
}

/// The general view: a one-line header, the guide and the command table.
fn overview() -> String {
    let mut out = format!(
        "rack {} guide (this text ships inside the binary; `rack update` brings the newest)\n\n{GUIDE}",
        env!("CARGO_PKG_VERSION")
    );
    out.push_str(
        "\n## Commands\n\n`rack guide COMMAND` (MCP: `racking_guide` with `command`) explains one in detail.\n\n",
    );
    for c in COMMANDS {
        let _ = write!(out, "- `rack {}`: {}", c.name, c.summary);
        if let Some(tool) = c.mcp {
            let _ = write!(out, " (MCP: `{tool}`)");
        }
        out.push('\n');
    }
    out
}

fn changes() -> String {
    format!("rack {}: what changed\n\n{NOTES}", env!("CARGO_PKG_VERSION"))
}

fn names() -> String {
    COMMANDS.iter().fold(String::new(), |mut out, c| {
        let _ = writeln!(out, "{}", c.name);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source of the `rack` binary, read so a command added there cannot escape these tests.
    const MAIN: &str = include_str!("main.rs");

    /// The `USAGE` text of the binary.
    fn usage() -> &'static str {
        let usage = MAIN.split("const USAGE: &str = \"\\\n").nth(1).expect("USAGE");
        usage.split("\";\n").next().expect("end of USAGE")
    }

    /// Commands of the command list in `USAGE`: the first word after `  rack `.
    fn usage_commands() -> Vec<&'static str> {
        let mut names: Vec<&str> = usage()
            .lines()
            .filter_map(|l| l.strip_prefix("  rack "))
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        names.dedup();
        names
    }

    /// Command names matched by the dispatch in `run`, aliases like `--version` left out.
    fn dispatch_commands() -> Vec<&'static str> {
        let body = MAIN.split("match command.as_str() {").nth(1).expect("dispatch");
        let body = body.split("other =>").next().expect("end of dispatch");
        body.lines()
            .filter(|l| l.contains("=>"))
            .filter_map(|l| l.split("=>").next())
            .flat_map(|arms| arms.split('"').skip(1).step_by(2))
            .filter(|n| !n.starts_with('-'))
            .collect()
    }

    fn sorted(commands: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = commands.iter().map(|c| (*c).to_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn overview_and_changes_name_this_version() {
        let version = env!("CARGO_PKG_VERSION");
        let overview = render(&Topic::Overview).expect("overview");
        assert!(overview.starts_with(&format!("rack {version} guide (this text ships inside the binary")));
        assert!(overview.contains("# Racking: guide for coding agents"));
        let changes = render(&Topic::Changes).expect("changes");
        assert!(changes.starts_with(&format!("rack {version}: what changed\n")));
    }

    #[test]
    fn overview_lists_every_command_with_its_tool() {
        let overview = render(&Topic::Overview).expect("overview");
        for c in COMMANDS {
            assert!(
                overview.contains(&format!("- `rack {}`: {}", c.name, c.summary)),
                "{}",
                c.name
            );
            if let Some(tool) = c.mcp {
                assert!(overview.contains(&format!("(MCP: `{tool}`)")), "{}", c.name);
            }
        }
    }

    #[test]
    fn list_prints_one_name_per_line() {
        let list = render(&Topic::List).expect("list");
        assert_eq!(list.lines().count(), COMMANDS.len());
        assert!(list.lines().any(|l| l == "get-attachment"));
    }

    #[test]
    fn unknown_command_names_the_valid_ones() {
        let err = render(&Topic::Command("nope")).expect_err("unknown");
        assert!(
            err.contains("`nope`") && err.contains("get-attachment, outbox"),
            "{err}"
        );
    }

    #[test]
    fn table_dispatch_and_usage_list_the_same_commands() {
        let table: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(
            sorted(&table),
            sorted(&dispatch_commands()),
            "table against the dispatch in main.rs"
        );
        assert_eq!(
            sorted(&table),
            sorted(&usage_commands()),
            "table against USAGE in main.rs"
        );
    }

    #[test]
    fn every_command_has_a_complete_help_file() {
        for c in COMMANDS {
            assert!(!c.summary.is_empty(), "{} has no summary", c.name);
            assert!(
                c.help.starts_with(&format!("# rack {}\n", c.name)),
                "docs/commands/{}.md must start with `# rack {}`",
                c.name,
                c.name
            );
            for section in ["Purpose", "Options", "Examples", "Exit codes", "MCP"] {
                assert!(
                    c.help.lines().any(|l| l == format!("## {section}")),
                    "docs/commands/{}.md has no `## {section}` section",
                    c.name
                );
            }
            let text = render(&Topic::Command(c.name)).expect("help");
            assert!(text.contains(c.name));
            if let Some(tool) = c.mcp {
                assert!(
                    c.help.contains(tool),
                    "docs/commands/{}.md does not name {tool}",
                    c.name
                );
            }
        }
    }

    /// Option names a command's parser accepts: the strings of its `Opts::parse(...)` call.
    fn parsed_options(command: &str) -> Vec<&'static str> {
        let function = match command {
            "update" => "self_update".to_owned(),
            other => other.replace('-', "_"),
        };
        let Some(body) = MAIN.split(&format!("\nfn {function}(")).nth(1) else {
            return Vec::new();
        };
        let body = body.split("\nfn ").next().expect("function end");
        let Some(call) = body.split("Opts::parse(").nth(1) else {
            return Vec::new();
        };
        let call = call.split(")?").next().expect("end of call");
        call.split('"').skip(1).step_by(2).collect()
    }

    #[test]
    fn every_option_a_command_parses_is_in_its_help() {
        let mut checked = 0;
        for c in COMMANDS {
            for option in parsed_options(c.name) {
                checked += 1;
                assert!(
                    c.help.contains(&format!("--{option}")),
                    "docs/commands/{}.md does not describe --{option}",
                    c.name
                );
            }
        }
        assert!(checked > 30, "option scan found only {checked}");
    }

    #[test]
    fn every_command_in_usage_is_in_the_guide() {
        // help and version explain themselves, and the guide is the text being read.
        let exempt = ["help", "version", "guide"];
        for name in usage_commands().into_iter().filter(|c| !exempt.contains(c)) {
            let tool = format!("racking_{}", name.replace('-', "_"));
            assert!(
                GUIDE.contains(&format!("rack {name}")) || GUIDE.contains(&tool),
                "docs/agent-guide.md does not mention the command `rack {name}` (or `{tool}`)"
            );
        }
    }

    /// Words after each "`rack " in `text` that are not commands of `USAGE`.
    fn unknown_commands(text: &str) -> Vec<String> {
        let commands = usage_commands();
        text.split("`rack ")
            .skip(1)
            .map(|part| {
                part.chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect::<String>()
            })
            // `rack --version` and `rack -V` are flags of the binary, not commands.
            .filter(|w| !w.is_empty() && !w.starts_with('-') && !commands.contains(&w.as_str()))
            .collect()
    }

    #[test]
    fn the_guide_names_no_command_that_usage_lacks() {
        assert_eq!(unknown_commands(GUIDE), Vec::<String>::new(), "in docs/agent-guide.md");
        for c in COMMANDS {
            assert_eq!(
                unknown_commands(c.help),
                Vec::<String>::new(),
                "in docs/commands/{}.md",
                c.name
            );
        }
    }
}
