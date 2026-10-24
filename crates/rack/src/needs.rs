//! What a job needs from its recipient (`rack send --needs`, `racking_send`
//! `needs`), checked against the capabilities the recipient published in
//! `GET /agents` before anything is sent. The check is advisory: capabilities
//! are written by each agent about itself and the server checks none of them.

use crate::client::{Agents, Client, RackError, Result};

/// Longest capability name the server takes.
const MAX_CHARS: usize = 32;

/// What to do when the recipient lacks a need.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnMissing {
    Refuse,
    SendAnyway,
}

/// Capability names as the server stores them: lowercased, 1 to 32 of `a-z0-9._-`.
/// `items` may each hold several names separated by commas; repeats are kept once.
pub fn parse<'a>(items: impl IntoIterator<Item = &'a str>) -> Result<Vec<String>> {
    let mut list: Vec<String> = Vec::new();
    for name in items.into_iter().flat_map(|i| i.split(',')) {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let ok = name.len() <= MAX_CHARS
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(RackError::Usage(format!(
                "capability `{name}` must be 1 to {MAX_CHARS} of a-z0-9._-"
            )));
        }
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Ok(list)
}

/// Checks `needs` against what `to` published, costing one `GET /agents`.
/// `Ok(None)`: all present, send quietly. `Ok(Some(warning))`: send after
/// showing the warning (nothing published to check, or `SendAnyway`).
/// `Err(RackError::Unmet)`: some are missing and `on_missing` refuses; do not send.
pub fn gate(c: &Client, to: &str, needs: &[String], on_missing: OnMissing) -> Result<Option<String>> {
    if needs.is_empty() {
        return Ok(None);
    }
    let asked = needs.join(", ");
    let published = match c.get::<Agents>("/agents") {
        Ok(all) => all.agents.into_iter().find(|a| a.name == to).map(|a| a.capabilities),
        // A server without presence has nothing to check against.
        Err(RackError::Api { status: 404, .. }) => None,
        Err(e) => return Err(e),
    };
    let has = match published {
        Some(has) if !has.is_empty() => has,
        _ => {
            return Ok(Some(format!(
                "{to} has published no capabilities, so the needs ({asked}) cannot be checked; sending anyway"
            )));
        }
    };
    let missing: Vec<&str> = needs.iter().filter(|n| !has.contains(n)).map(String::as_str).collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let lacks = format!(
        "{to} has not published {} (it lists: {})",
        missing.join(", "),
        has.join(", ")
    );
    match on_missing {
        OnMissing::SendAnyway => Ok(Some(format!("{lacks}; sending anyway, as forced"))),
        OnMissing::Refuse => Err(RackError::Unmet(format!(
            "{lacks}; not sent\nhint: send it to an agent that lists them (rack agents, racking_agents), or send \
             anyway with --force (racking_send: force true). Capabilities are what each agent says about itself, \
             so this check is advice, not a guarantee."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists() {
        let got = parse(["NVIDIA, windows", "kb,,nvidia"]).ok();
        assert_eq!(
            got,
            Some(vec!["nvidia".to_owned(), "windows".to_owned(), "kb".to_owned()])
        );
        assert_eq!(parse([""]).ok(), Some(Vec::new()));
        assert!(parse(["intel gpu"]).is_err());
        assert!(parse([&*"c".repeat(MAX_CHARS + 1)]).is_err());
    }
}
