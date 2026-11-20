//! The reference the hub serves without a session (`reference`): how to write cfg scripts and
//! profiles (`docs/cfg-guide.md`), the Tcl command reference (`docs/commands.md`, generated from
//! the worker's `help` table) and the aurix-dbg cfg scripts a board cfg loads, which are not
//! installed with the hub.

use crate::types::Reference;

const CFG_GUIDE: &str = include_str!("../../../docs/cfg-guide.md");
const COMMANDS: &str = include_str!("../../../docs/commands.md");

/// The aurix-dbg cfg scripts, by file name.
const FILES: &[(&str, &str, &str)] = &[
    ("tc3xx_ocds.cfg", "generic TC3xx part every board cfg loads", include_str!("../../../../tc3xx_ocds.cfg")),
    ("fs26_sbc.cfg", "NXP FS26 safety-SBC policy (set tc3xx_sbc fs26)", include_str!("../../../../fs26_sbc.cfg")),
    ("tc375_lite_example.cfg", "example board cfg without a safety SBC", include_str!("../../../../tc375_lite_example.cfg")),
    ("tc37x_fs26_example.cfg", "example board cfg with an FS26", include_str!("../../../../tc37x_fs26_example.cfg")),
];

/// One command of `commands.md`.
struct Command {
    section: String,
    name: String,
    /// The command line with its arguments.
    usage: String,
    summary: String,
    /// The whole entry (markdown).
    text: String,
}

fn commands() -> Vec<Command> {
    let doc = COMMANDS.replace("\r\n", "\n");
    let mut out: Vec<Command> = Vec::new();
    let mut section = String::new();
    for line in doc.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            section = title.to_string();
            continue;
        }
        if let Some(head) = line.strip_prefix("### ") {
            let name = head.trim_matches('`').to_string();
            out.push(Command { section: section.clone(), name, usage: String::new(), summary: String::new(), text: String::new() });
            continue;
        }
        let Some(c) = out.last_mut() else { continue };
        if section != c.section {
            continue;
        }
        c.text.push_str(line);
        c.text.push('\n');
    }
    for c in &mut out {
        c.text = c.text.trim().to_string();
        // ```\n<usage>\n```\n\n<summary>\n\n<details>
        let mut parts = c.text.splitn(3, "```");
        parts.next();
        c.usage = parts.next().unwrap_or_default().trim().to_string();
        c.summary = parts.next().unwrap_or_default().trim().lines().next().unwrap_or_default().to_string();
        c.text = format!("### `{}`\n\n{}", c.name, c.text);
    }
    out
}

fn index() -> String {
    let mut out = String::from(
        "Topics of `reference`:\n\n\
         - `cfg`: how to write a board cfg and a hub profile: the script layers, the settings of tc3xx_ocds.cfg, \
         probes, events, how to try a cfg. Read this first.\n\
         - `commands`: every Tcl command with its arguments, by section.\n\
         - a command (`reset_config`, `suspend add`, `attach_mode`): its full description and its subcommands.\n\
         - any other word (`srst`, `watchdog`): the commands whose description mentions it.\n\
         - a file name: the text of an aurix-dbg cfg script, to save next to the board cfg:\n",
    );
    for (name, what, _) in FILES {
        out.push_str(&format!("  - `{name}`: {what}\n"));
    }
    out
}

fn command_list(cmds: &[Command]) -> String {
    let mut out = String::from("Tcl commands (`reference` with a command name for its description):\n");
    let mut section = "";
    for c in cmds {
        if c.section != section {
            section = &c.section;
            out.push_str(&format!("\n## {section}\n"));
        }
        out.push_str(&format!("- `{}`: {}\n", c.usage, c.summary));
    }
    out
}

/// The answer to `reference` for `topic`.
pub fn lookup(topic: &str) -> Reference {
    let topic = topic.trim();
    let answer = |text: String| Reference { topic: topic.to_string(), text };
    let key = topic.to_lowercase();
    match key.as_str() {
        "" | "index" | "topics" => return answer(index()),
        "cfg" | "guide" | "profile" | "profiles" => return answer(CFG_GUIDE.replace("\r\n", "\n")),
        _ => {}
    }
    if let Some((_, _, text)) = FILES.iter().find(|(name, _, _)| name.eq_ignore_ascii_case(&key) || name.trim_end_matches(".cfg") == key) {
        return answer(text.replace("\r\n", "\n"));
    }
    let cmds = commands();
    if key == "commands" {
        return answer(command_list(&cmds));
    }
    // The command itself and its subcommands: `adapter` -> `adapter driver`, `adapter speed`, ...
    let named: Vec<&Command> = cmds.iter().filter(|c| c.name == topic || c.name.starts_with(&format!("{topic} "))).collect();
    if !named.is_empty() {
        return answer(named.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\n\n"));
    }
    let found: Vec<&Command> = cmds.iter().filter(|c| c.name != "help" && c.text.to_lowercase().contains(&key)).collect();
    answer(match found.len() {
        0 => format!("Nothing matches '{topic}'.\n\n{}", index()),
        1 => found[0].text.clone(),
        _ => {
            let mut out = format!("Commands mentioning '{topic}' (`reference` with a command name for its description):\n");
            for c in found {
                out.push_str(&format!("- `{}`: {}\n", c.usage, c.summary));
            }
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics() {
        assert!(lookup("").text.contains("tc3xx_ocds.cfg"));
        assert!(lookup("cfg").text.contains("## Profiles"));
        assert!(lookup("tc3xx_ocds.cfg").text.contains("proc tc3xx_setup"));
        assert!(lookup("fs26_sbc").text.contains("proc sbc_reset_hook"));
        let all = lookup("commands").text;
        assert!(all.contains("## Reset") && all.contains("- `reset_config [<option> ...]`: Set the reset options"), "{all}");
        assert!(!all.contains("## Contents"));
    }

    #[test]
    fn commands_and_search() {
        let rc = lookup("reset_config").text;
        assert!(rc.starts_with("### `reset_config`") && rc.contains("connect_assert_srst"), "{rc}");
        let adapter = lookup("adapter").text;
        assert!(adapter.contains("### `adapter driver`") && adapter.contains("### `adapter debug_password`"));
        assert!(!adapter.contains("### `reset_config`"));
        assert!(lookup("srst").text.contains("`reset_config"));
        assert!(lookup("no-such-thing-at-all").text.starts_with("Nothing matches"));
        // Every entry got its usage and summary.
        for c in commands() {
            assert!(!c.usage.is_empty() && !c.summary.is_empty(), "{}", c.name);
        }
    }
}
