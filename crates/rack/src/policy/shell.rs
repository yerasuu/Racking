//! Reads the command line of a Bash tool call as a plain argv, refusing every piece of shell syntax
//! that could run something else: separators, pipes, redirections, substitutions, expansions,
//! globs. A command that needs any of those is not one a profile allows, so refusing them costs
//! nothing and removes the whole class of "the pattern matched but the shell did more" tricks.

/// A command line as the shell would run it, once it is known to hold nothing but words.
#[derive(Debug, PartialEq, Eq)]
pub struct Argv {
    /// `NAME=value` words before the command, in order.
    pub env: Vec<(String, String)>,
    /// The command and its arguments; never empty.
    pub args: Vec<String>,
}

/// Characters a word may hold outside quotes: the ones the shell gives no meaning to.
fn plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
}

struct Word {
    text: String,
    /// Byte length of the leading unquoted `NAME=` part, when the word starts with one.
    assignment: Option<usize>,
    unquoted_so_far: bool,
}

impl Word {
    fn new() -> Self {
        Self {
            text: String::new(),
            assignment: None,
            unquoted_so_far: true,
        }
    }

    fn push_plain(&mut self, c: char) {
        if c == '=' && self.unquoted_so_far && self.assignment.is_none() && is_name(&self.text) {
            self.assignment = Some(self.text.len());
        }
        self.text.push(c);
    }
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Splits `line` into words.
///
/// # Errors
///
/// Any character with a meaning to the shell outside quotes (`;`, `|`, `&`, `<`, `>`, `(`, `$`, a
/// backquote, a backslash, a glob, `~`, `#`, a newline...), `$`, backquotes, backslashes or `!`
/// inside double quotes, an unterminated quote, a control character, or an empty command.
pub fn parse(line: &str) -> Result<Argv, String> {
    let mut words: Vec<Word> = Vec::new();
    let mut word: Option<Word> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => words.extend(word.take()),
            '\'' => {
                let w = word.get_or_insert_with(Word::new);
                w.unquoted_so_far = false;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) if c.is_control() => return Err(control(c)),
                        Some(c) => w.text.push(c),
                        None => return Err("unterminated single quote".to_owned()),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(Word::new);
                w.unquoted_so_far = false;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(c @ ('$' | '`' | '\\' | '!')) => {
                            return Err(format!("`{c}` inside double quotes is shell syntax; use single quotes"));
                        }
                        Some(c) if c.is_control() => return Err(control(c)),
                        Some(c) => w.text.push(c),
                        None => return Err("unterminated double quote".to_owned()),
                    }
                }
            }
            c if plain(c) => word.get_or_insert_with(Word::new).push_plain(c),
            c if c.is_control() => return Err(control(c)),
            c => {
                return Err(format!(
                    "`{c}` is shell syntax; a profile runs one command with plain arguments, no shell"
                ));
            }
        }
    }
    words.extend(word);
    let mut env = Vec::new();
    let mut args = Vec::with_capacity(words.len());
    for w in words {
        match w.assignment {
            Some(at) if args.is_empty() => {
                let (name, value) = w.text.split_at(at);
                env.push((name.to_owned(), value[1..].to_owned()));
            }
            _ => args.push(w.text),
        }
    }
    if args.is_empty() {
        return Err("no command to run".to_owned());
    }
    Ok(Argv { env, args })
}

fn control(c: char) -> String {
    format!("control character U+{:04X} in the command", u32::from(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        parse(line).expect(line).args
    }

    #[test]
    fn plain_words_and_quotes() {
        assert_eq!(args("/usr/bin/true --ok"), ["/usr/bin/true", "--ok"]);
        assert_eq!(args("  a   'b c'  \"d e\" "), ["a", "b c", "d e"]);
        assert_eq!(args("a 'x;y|z$(w)'"), ["a", "x;y|z$(w)"], "single quotes are literal");
        assert_eq!(args("a ''"), ["a", ""]);
        assert_eq!(args("a --n='5 6'"), ["a", "--n=5 6"]);
    }

    #[test]
    fn shell_syntax_is_refused() {
        for line in [
            "a; b",
            "a && b",
            "a | b",
            "a > f",
            "a < f",
            "a $(b)",
            "a `b`",
            "a $HOME",
            "a \"$HOME\"",
            "a *.txt",
            "a ~/x",
            "a # c",
            "a\nb",
            "a\\ b",
            "(a)",
            "a &",
            "a {x,y}",
            "a 'open",
            "",
            "   ",
        ] {
            assert!(parse(line).is_err(), "{line:?} must be refused");
        }
    }

    #[test]
    fn leading_assignments_are_env() {
        let got = parse("WINEPREFIX=/p DEBUG='a b' /usr/bin/wine x=1").expect("parse");
        assert_eq!(
            got.env,
            [
                ("WINEPREFIX".to_owned(), "/p".to_owned()),
                ("DEBUG".to_owned(), "a b".to_owned())
            ]
        );
        assert_eq!(got.args, ["/usr/bin/wine", "x=1"]);
        assert_eq!(args("'A=b' c"), ["A=b", "c"], "a quoted assignment is a command word");
        assert!(parse("A=b").is_err(), "assignments alone run nothing");
    }
}
