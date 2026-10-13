//! Reads the selection menu a Claude pane is holding open.
//!
//! Claude Code draws its questions (`AskUserQuestion`), permission prompts,
//! plan approval, and the folder-trust dialog as a select list in place of the
//! composer. None of them reach the transcript until they're answered, so the
//! pane is the only place an open menu shows up. While one is open, pasted
//! text is dropped and Enter picks whichever option is highlighted.

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::status::AgentKind;

/// The glyph Claude draws beside the highlighted option.
const CURSOR: char = '❯';
const MAX_OPTIONS: usize = 24;
const MAX_PROMPT_ROWS: usize = 60;
/// How many rows from the bottom of a capture the parser reads.
const MAX_SCAN_ROWS: usize = 240;
const MAX_PROMPT_BYTES: usize = 8 * 1024;
const MAX_LABEL_BYTES: usize = 512;
const MAX_DETAIL_BYTES: usize = 2 * 1024;
const MAX_HINT_BYTES: usize = 512;
/// Phrases in the key-hint row Claude prints under a menu. The composer's
/// footer never carries any of them.
const HINT_MARKERS: [&str; 5] = [
    "esc to cancel",
    "enter to select",
    "enter to confirm",
    "to navigate",
    "ctrl+g to edit",
];
/// Placeholder labels of the options that become a text field when
/// highlighted. Typing replaces the label with the typed text.
const TEXT_ENTRY_LABELS: [&str; 2] = ["type something", "tell claude what to change"];
/// Characters that only draw boxes and separators.
const RULE_CHARS: &str = "─━┄┅┈┉╌╍│┃┌┐└┘├┤┬┴┼╭╮╯╰═║╔╗╚╝╠╣╦╩╬▁▔";
/// Answer-state marks in a multi-question header. They change as questions
/// are answered, so they stay out of the menu id.
const STATE_MARKS: &str = "☐☒✔✓";

/// A selection menu an agent is holding open, as its pane shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PendingChoice {
    /// Names this question and its options, ignoring the highlight and
    /// checkbox marks. An answer carries it so it can't land on a different
    /// menu that opened in the meantime.
    pub id: String,
    /// The dialog text above the options, such as the question or the
    /// command awaiting permission.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    pub options: Vec<ChoiceOption>,
    /// Index of the highlighted option.
    pub selected: usize,
    /// The key hints drawn under the menu.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

/// One row the menu highlight can rest on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChoiceOption {
    /// The number printed before the option. Absent on rows such as a
    /// multi-select `Next` and on unnumbered menus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    pub label: String,
    /// Description or wrapped text printed under the label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// A multi-select checkbox's state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// Highlighting this option opens a text field for a typed answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_entry: bool,
}

/// One captured row, with the highlight glyph replaced by the blank Claude
/// draws in its place on every other row.
struct Row<'a> {
    indent: usize,
    text: &'a str,
    cursor: bool,
    /// The row draws a box edge or separator and nothing else.
    rule: bool,
    /// The row is a separator that starts at the left edge, which is how
    /// Claude opens a dialog. Indented separators divide sections inside one.
    border: bool,
}

/// The rows a menu occupies and what they offer.
struct Block {
    start: usize,
    end: usize,
    options: Vec<ChoiceOption>,
    selected: usize,
    numbered: bool,
}

/// Finds the menu at the bottom of a Claude pane, if one is open.
///
/// A menu needs a highlighted option with at least one sibling, and nothing
/// under it but Claude's key hints. The composer never qualifies: its footer
/// isn't a hint row, so a typed `1.`/`2.` list stays a draft.
#[must_use]
pub fn pending_choice(kind: AgentKind, content: &str) -> Option<PendingChoice> {
    if kind != AgentKind::Claude {
        return None;
    }
    // A menu sits at the bottom of the pane, and its prompt walk stops within
    // `MAX_PROMPT_ROWS`, so older scrollback can never matter.
    let mut rows = content
        .lines()
        .rev()
        .take(MAX_SCAN_ROWS)
        .map(row)
        .collect::<Vec<_>>();
    rows.reverse();
    let last = rows.iter().rposition(|row| !row.text.is_empty())?;
    let rows = &rows[..=last];
    let cursor = rows.iter().rposition(|row| row.cursor)?;
    let block = numbered_block(rows, cursor).or_else(|| plain_block(rows, cursor))?;

    let below = rows[block.end + 1..]
        .iter()
        .filter(|row| !row.text.is_empty())
        .collect::<Vec<_>>();
    let hint = match below.as_slice() {
        [] if block.numbered => String::new(),
        [first, ..] if below.len() <= 2 && is_hint(first.text) => {
            let joined = below.iter().map(|row| row.text).collect::<Vec<_>>();
            clip(&joined.join("\n"), MAX_HINT_BYTES)
        }
        _ => return None,
    };

    let prompt = prompt_text(rows, block.start);
    Some(PendingChoice {
        id: menu_id(&prompt, &block.options),
        prompt,
        options: block.options,
        selected: block.selected,
        hint,
    })
}

fn row(line: &str) -> Row<'_> {
    let start = line.trim_start_matches(is_blank);
    let lead = line[..line.len() - start.len()].chars().count();
    let rule = !start.trim_end_matches(is_blank).is_empty()
        && start
            .trim_end_matches(is_blank)
            .chars()
            .all(|character| RULE_CHARS.contains(character));
    let border = rule && ((lead == 0 && start.starts_with(['─', '━'])) || start.starts_with('▔'));
    if let Some(rest) = start.strip_prefix(CURSOR) {
        let text = rest.trim_start_matches(is_blank);
        let gap = rest[..rest.len() - text.len()].chars().count();
        return Row {
            indent: lead + 1 + gap,
            text: text.trim_end_matches(is_blank),
            cursor: true,
            rule: false,
            border: false,
        };
    }
    Row {
        indent: lead,
        text: start.trim_end_matches(is_blank),
        cursor: false,
        rule,
        border,
    }
}

fn is_blank(character: char) -> bool {
    character == ' ' || character == '\u{a0}'
}

fn is_hint(text: &str) -> bool {
    let lower = text.to_lowercase();
    HINT_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Splits `3. Blue` into its number and the text after `3. `.
fn enumerated(text: &str) -> Option<(u32, &str)> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let label = text[digits..].strip_prefix(". ")?;
    Some((text[..digits].parse().ok()?, label))
}

/// Splits a multi-select checkbox off the front of a label.
fn checkbox(label: &str) -> (Option<bool>, &str) {
    for (mark, checked) in [
        ("[ ] ", false),
        ("[✔] ", true),
        ("[✓] ", true),
        ("[x] ", true),
    ] {
        if let Some(rest) = label.strip_prefix(mark) {
            return (Some(checked), rest);
        }
    }
    (None, label)
}

fn option(number: Option<u32>, label: &str, checked: Option<bool>) -> ChoiceOption {
    let label = label.trim();
    let placeholder = label.trim_end_matches('.').to_lowercase();
    ChoiceOption {
        number,
        label: clip(label, MAX_LABEL_BYTES),
        detail: String::new(),
        checked,
        text_entry: TEXT_ENTRY_LABELS.contains(&placeholder.as_str()),
    }
}

fn add_detail(option: &mut ChoiceOption, text: &str) {
    if !option.detail.is_empty() {
        option.detail.push('\n');
    }
    option.detail.push_str(text);
    option.detail = clip(&option.detail, MAX_DETAIL_BYTES);
}

/// Reads a numbered menu around the highlighted row.
///
/// Options share one column. Rows indented past an option's label are its
/// description; rows between the number column and the label are extra stops
/// such as a multi-select `Next`. A separator can split the list, as it does
/// above `Chat about this`.
fn numbered_block(rows: &[Row<'_>], cursor: usize) -> Option<Block> {
    let anchor = if enumerated(rows[cursor].text).is_some() {
        cursor
    } else {
        // The highlight can rest on an extra stop under the options.
        let mut index = cursor;
        loop {
            index = index.checked_sub(1)?;
            let row = &rows[index];
            if row.text.is_empty() || row.rule {
                return None;
            }
            if row.indent < rows[cursor].indent {
                if enumerated(row.text).is_some() {
                    break index;
                }
                return None;
            }
        }
    };
    let column = rows[anchor].indent;
    let member = |index: usize| -> bool {
        let row = &rows[index];
        !row.text.is_empty()
            && !row.rule
            && (row.indent > column || (row.indent == column && enumerated(row.text).is_some()))
    };
    let option_at = |index: usize| -> bool {
        rows.get(index)
            .is_some_and(|row| row.indent == column && enumerated(row.text).is_some())
    };
    let mut start = anchor;
    while start > 0
        && (member(start - 1)
            || (rows[start - 1].rule && option_at(start) && start >= 2 && member(start - 2)))
    {
        start -= 1;
    }
    while !option_at(start) {
        start += 1;
    }
    let mut end = anchor;
    while end + 1 < rows.len() && (member(end + 1) || (rows[end + 1].rule && option_at(end + 2))) {
        end += 1;
    }

    let mut options: Vec<ChoiceOption> = Vec::new();
    let mut label_column = column;
    let mut selected = None;
    for row in &rows[start..=end] {
        if row.rule {
            continue;
        }
        if row.indent == column
            && let Some((number, rest)) = enumerated(row.text)
        {
            let (checked, label) = checkbox(rest);
            label_column = column + row.text.chars().count() - label.chars().count();
            options.push(option(Some(number), label, checked));
        } else if row.indent >= label_column && !row.cursor {
            add_detail(options.last_mut()?, row.text);
        } else {
            options.push(option(None, row.text, None));
        }
        if row.cursor {
            selected = Some(options.len() - 1);
        }
        if options.len() > MAX_OPTIONS {
            return None;
        }
    }
    let numbers = options.iter().filter_map(|option| option.number);
    if !numbers
        .clone()
        .eq(1..=u32::try_from(numbers.clone().count()).ok()?)
        || numbers.count() < 2
    {
        return None;
    }
    Some(Block {
        start,
        end,
        options,
        selected: selected?,
        numbered: true,
    })
}

/// Reads an unnumbered menu, such as the folder-trust dialog: sibling rows in
/// the highlighted row's column, each optionally followed by deeper rows.
fn plain_block(rows: &[Row<'_>], cursor: usize) -> Option<Block> {
    let column = rows[cursor].indent;
    let member = |row: &Row<'_>| !row.text.is_empty() && !row.rule && row.indent >= column;
    let mut start = cursor;
    while start > 0 && member(&rows[start - 1]) {
        start -= 1;
    }
    while rows[start].indent != column {
        start += 1;
    }
    let mut end = cursor;
    while end + 1 < rows.len() && member(&rows[end + 1]) {
        end += 1;
    }
    let mut options: Vec<ChoiceOption> = Vec::new();
    let mut selected = None;
    for row in &rows[start..=end] {
        if row.indent == column {
            options.push(option(None, row.text, None));
        } else {
            add_detail(options.last_mut()?, row.text);
        }
        if row.cursor {
            selected = Some(options.len() - 1);
        }
    }
    if options.len() < 2 || options.len() > MAX_OPTIONS {
        return None;
    }
    Some(Block {
        start,
        end,
        options,
        selected: selected?,
        numbered: false,
    })
}

/// Collects the dialog text above the options, back to the separator that
/// opens the dialog. Without one in reach, only the paragraph right above the
/// options counts, so scrollback never leaks in.
fn prompt_text(rows: &[Row<'_>], start: usize) -> String {
    let mut collected = Vec::new();
    let mut opened = false;
    for row in rows[..start].iter().rev().take(MAX_PROMPT_ROWS) {
        if row.border || row.cursor {
            opened = true;
            break;
        }
        collected.push(row);
    }
    if !opened {
        let paragraph = collected
            .iter()
            .skip_while(|row| row.text.is_empty())
            .take_while(|row| !row.text.is_empty())
            .count();
        let blank = collected
            .iter()
            .take_while(|row| row.text.is_empty())
            .count();
        collected.truncate(blank + paragraph);
    }
    collected.reverse();
    let margin = collected
        .iter()
        .filter(|row| !row.text.is_empty() && !row.rule)
        .map(|row| row.indent)
        .min()
        .unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    for row in collected {
        if row.text.is_empty() || row.rule {
            if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        lines.push(format!("{}{}", " ".repeat(row.indent - margin), row.text));
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    clip(&lines.join("\n"), MAX_PROMPT_BYTES)
}

fn menu_id(prompt: &str, options: &[ChoiceOption]) -> String {
    let mut digest = Sha256::new();
    let stable = prompt
        .chars()
        .filter(|character| !STATE_MARKS.contains(*character))
        .collect::<String>();
    digest.update(stable.as_bytes());
    for option in options {
        digest.update([0]);
        digest.update(option.number.unwrap_or(0).to_be_bytes());
        digest.update(option.label.as_bytes());
        digest.update([0]);
        digest.update(option.detail.as_bytes());
    }
    let finished = digest.finalize();
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&finished[..8]);
    format!("{:016x}", u64::from_be_bytes(prefix))
}

/// Shortens text to at most `limit` bytes on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The longest typed answer atmux types into a menu's text field.
pub const MAX_ANSWER_TEXT_BYTES: usize = 4 * 1024;
/// How many times answering re-reads the pane for one key to land.
const SETTLE_READS: usize = 60;

/// A key answering a menu presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuKey {
    Up,
    Down,
    Enter,
    Escape,
}

/// What answering a menu needs from the pane that shows it.
pub(crate) trait MenuPane {
    /// Captures the pane the same way the owner's poll does, so the menu id
    /// comes out the same.
    fn read(&mut self) -> anyhow::Result<String>;
    fn press(&mut self, key: MenuKey) -> anyhow::Result<()>;
    /// Pastes text without submitting it.
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;
    /// Gives the agent a moment to redraw before the next read.
    fn pause(&mut self);
}

/// Why an answer wasn't delivered.
#[derive(Debug)]
pub(crate) enum AnswerError {
    /// The menu changed or closed before the answer landed.
    Stale(&'static str),
    /// The answer doesn't fit the menu it names.
    Invalid(&'static str),
    /// Reading or driving the pane failed.
    Pane(anyhow::Error),
}

impl From<anyhow::Error> for AnswerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Pane(error)
    }
}

/// Checks typed answer text before anything touches the pane.
///
/// # Errors
///
/// Returns a message for empty, oversized, or multi-line text. A newline
/// would reach the agent as Enter.
pub fn validate_answer_text(text: &str) -> Result<(), &'static str> {
    if text.trim().is_empty() {
        return Err("type an answer first");
    }
    if text.len() > MAX_ANSWER_TEXT_BYTES {
        return Err("the answer is longer than 4 KiB");
    }
    if text.chars().any(char::is_control) {
        return Err("the answer must be one line of text");
    }
    Ok(())
}

/// Picks `option` in the menu named `menu`, or closes the menu with Esc when
/// `option` is `None`.
///
/// The menu is read again first, so an answer chosen from a menu that has
/// since closed or changed is refused rather than landing on its successor.
/// The highlight then moves one key at a time, with the pane re-read after
/// each key, and Enter goes only once it rests on the chosen option. A
/// text-entry option gets `text` typed into it first.
pub(crate) fn answer(
    pane: &mut impl MenuPane,
    kind: AgentKind,
    menu: &str,
    option: Option<usize>,
    text: Option<&str>,
) -> Result<(), AnswerError> {
    let stale = "the menu changed or closed before the answer arrived";
    let mut current = pending_choice(kind, &pane.read()?)
        .filter(|current| current.id == menu)
        .ok_or(AnswerError::Stale(stale))?;
    let Some(option) = option else {
        if text.is_some() {
            return Err(AnswerError::Invalid("cancelling a menu takes no text"));
        }
        pane.press(MenuKey::Escape)?;
        return Ok(());
    };
    let target = current
        .options
        .get(option)
        .ok_or(AnswerError::Invalid("that option isn't in this menu"))?;
    match (target.text_entry, text) {
        (true, None) => return Err(AnswerError::Invalid("type an answer for this option")),
        (false, Some(_)) => {
            return Err(AnswerError::Invalid("this option doesn't take typed text"));
        }
        (_, Some(text)) => validate_answer_text(text).map_err(AnswerError::Invalid)?,
        (false, None) => {}
    }
    for _ in 0..current.options.len() {
        if current.selected == option {
            break;
        }
        let from = current.selected;
        pane.press(if from < option {
            MenuKey::Down
        } else {
            MenuKey::Up
        })?;
        current = settle(pane, kind, |next| next.id == menu && next.selected != from)
            .ok_or(AnswerError::Stale(stale))?;
    }
    if current.selected != option {
        return Err(AnswerError::Stale(
            "the highlight never reached that option",
        ));
    }
    if let Some(text) = text {
        pane.type_text(text)?;
        // The placeholder gives way to the typed text; nothing else moves.
        let before = current.clone();
        settle(pane, kind, |next| {
            next.selected == option
                && next.options.len() == before.options.len()
                && next.options.iter().zip(&before.options).enumerate().all(
                    |(index, (now, was))| {
                        if index == option {
                            now.label != was.label
                        } else {
                            now.label == was.label
                        }
                    },
                )
        })
        .ok_or(AnswerError::Stale("the menu didn't take the typed answer"))?;
        // Typing into a multi-select's field already ticks it. Enter would
        // untick it again.
        if before.options[option].checked.is_some() {
            return Ok(());
        }
    }
    pane.press(MenuKey::Enter)?;
    Ok(())
}

/// Re-reads the pane until `ready` accepts the menu, or gives up.
fn settle(
    pane: &mut impl MenuPane,
    kind: AgentKind,
    ready: impl Fn(&PendingChoice) -> bool,
) -> Option<PendingChoice> {
    for _ in 0..SETTLE_READS {
        pane.pause();
        let content = pane.read().ok()?;
        if let Some(next) = pending_choice(kind, &content).filter(|next| ready(next)) {
            return Some(next);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `tmux capture-pane -p` tails from Claude Code 2.1.296, with long
    /// separators shortened to 40 characters.
    mod panes {
        pub const QUESTION: &str = concat!(
            "❯ Use the AskUserQuestion tool right now to ask me one single-select question: \"Which fruit should we use?\"\n",
            "  with header \"Fruit\" and options Apple (Crunchy red fruit), Banana (Soft yellow fruit), Cherry (Small stone\n",
            "  fruit). Do nothing else.\n",
            "────────────────────────────────────────\n",
            " ☐ Fruit\n",
            "\n",
            "Which fruit should we use?\n",
            "\n",
            "❯ 1. Apple\n",
            "     Crunchy red fruit\n",
            "  2. Banana\n",
            "     Soft yellow fruit\n",
            "  3. Cherry\n",
            "     Small stone fruit\n",
            "  4. Type something.\n",
            "────────────────────────────────────────\n",
            "  5. Chat about this\n",
            "\n",
            "Enter to select · ↑/↓ to navigate · Esc to cancel\n",
        );
        pub const TRUST: &str = concat!(
            "────────────────────────────────────────\n",
            " Accessing workspace:\n",
            "\n",
            " /home/me/project\n",
            "\n",
            " Quick safety check: Is this a project you created or one you trust?\n",
            "\n",
            " Security guide\n",
            "\n",
            " ❯ No, exit\n",
            "   Yes, I trust this folder\n",
            "\n",
            " Enter to confirm · Esc to cancel\n",
        );
        pub const COMPOSER_IDLE: &str = concat!(
            "● You picked Banana for the fruit question.\n",
            "\n",
            "✻ Worked for 2s · done 1:34 PM\n",
            "\n",
            "────────────────────────────────────────\n",
            "❯\u{a0}\n",
            "────────────────────────────────────────\n",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents\n",
        );
        /// An operator's numbered draft looks like a menu until the footer.
        pub const COMPOSER_NUMBERED_DRAFT: &str = concat!(
            "────────────────────────────────────────\n",
            "❯ 1. fix the parser\n",
            "  2. add a test\n",
            "────────────────────────────────────────\n",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents\n",
        );
        pub const PERMISSION: &str = concat!(
            "❯ Run the shell command: touch probe-file.txt\n",
            "\n",
            "  Creating empty probe-file.txt in working directory\n",
            "  ⎿  $ touch probe-file.txt\n",
            "\n",
            "────────────────────────────────────────\n",
            " Bash command\n",
            " Tip: auto mode handles these prompts for you — choose \"switch to auto mode\" below\n",
            " Create empty probe-file.txt in working directory\n",
            "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n",
            " touch probe-file.txt\n",
            "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n",
            " Do you want to proceed?\n",
            " ❯ 1. Yes\n",
            "   2. Yes, and always allow access to /mnt/data/claude-tmp/hd/claude-1000/-home-ryan-IdeaProjects-atmux/55b434\n",
            "      be-f2ee-4232-bf66-55238d1c251e/scratchpad/choice-probe from this project\n",
            "   3. Yes, and switch to auto mode · auto mode handles these prompts for you\n",
            "   4. No\n",
            "\n",
            " Esc to cancel · Tab to amend\n",
        );
        pub const PLAN: &str = concat!(
            "  ⎿ \u{a0}/plan to preview\n",
            "▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n",
            "\n",
            "  ────────────────────────────────────────\n",
            "   Ready to code?\n",
            "\n",
            "   Here is Claude's plan:\n",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n",
            "   1. Create hello.txt in the working directory.\n",
            "   2. Write the content hi to it.\n",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "\n",
            "  ────────────────────────────────────────\n",
            "   Claude has written up a plan and is ready to execute. Would you like to proceed?\n",
            "\n",
            "   ❯ 1. Yes, and use auto mode\n",
            "     2. Yes, manually approve edits\n",
            "     3. Tell Claude what to change\n",
            "        shift+tab to approve with this feedback\n",
            "\n",
            "   ctrl+g to edit in Vim · ~/.claude-hd/plans/plan-create-a-file-snuggly-babbage.md\n",
        );
        pub const MULTI_NEXT: &str = concat!(
            "❯ Use AskUserQuestion once with TWO questions: first \"Which colors?\" header \"Colors\" multiSelect true,\n",
            "  options Red (warm), Green (calm), Blue (cool); second \"Which size?\" header \"Size\" single-select, options\n",
            "  Small (tiny), Large (big). Do nothing else.\n",
            "────────────────────────────────────────\n",
            "←  ☐ Colors  ☐ Size  ✔ Submit  →\n",
            "\n",
            "Which colors?\n",
            "\n",
            "  1. [ ] Red\n",
            "         Warm\n",
            "  2. [ ] Green\n",
            "         Calm\n",
            "  3. [ ] Blue\n",
            "         Cool\n",
            "  4. [ ] Type something\n",
            "❯    Next\n",
            "────────────────────────────────────────\n",
            "  5. Chat about this\n",
            "\n",
            "Enter to select · Tab/Arrow keys to navigate · ctrl+g to edit in Vim · Esc to cancel\n",
        );
        pub const REVIEW: &str = concat!(
            "❯ Use AskUserQuestion once with TWO questions: first \"Which colors?\" header \"Colors\" multiSelect true,\n",
            "  options Red (warm), Green (calm), Blue (cool); second \"Which size?\" header \"Size\" single-select, options\n",
            "  Small (tiny), Large (big). Do nothing else.\n",
            "\n",
            "────────────────────────────────────────\n",
            "←  ☒ Colors  ☒ Size  ✔ Submit  →\n",
            "\n",
            "Review your answers\n",
            "\n",
            " ● Which colors?\n",
            "   → Purple\n",
            " ● Which size?\n",
            "   → Medium\n",
            "\n",
            "Ready to submit your answers?\n",
            "\n",
            "❯ 1. Submit answers\n",
            "  2. Cancel\n",
        );
        pub const TYPED_ANSWER: &str = concat!(
            "❯ Use AskUserQuestion once with TWO questions: first \"Which colors?\" header \"Colors\" multiSelect true,\n",
            "  options Red (warm), Green (calm), Blue (cool); second \"Which size?\" header \"Size\" single-select, options\n",
            "  Small (tiny), Large (big). Do nothing else.\n",
            "────────────────────────────────────────\n",
            "←  ☒ Colors  ☐ Size  ✔ Submit  →\n",
            "\n",
            "Which size?\n",
            "\n",
            "  1. Small\n",
            "     Tiny\n",
            "  2. Large\n",
            "     Big\n",
            "❯ 3. Medium\n",
            "────────────────────────────────────────\n",
            "  4. Chat about this\n",
            "\n",
            "Enter to select · Tab/Arrow keys to navigate · ctrl+g to edit in Vim · Esc to cancel\n",
        );
    }

    fn labels(choice: &PendingChoice) -> Vec<&str> {
        choice
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect()
    }

    #[test]
    fn reads_a_single_select_question() {
        let choice = pending_choice(AgentKind::Claude, panes::QUESTION).expect("menu");
        assert_eq!(
            labels(&choice),
            [
                "Apple",
                "Banana",
                "Cherry",
                "Type something.",
                "Chat about this"
            ]
        );
        assert_eq!(choice.selected, 0);
        assert_eq!(choice.prompt, " ☐ Fruit\n\nWhich fruit should we use?");
        assert_eq!(
            choice.hint,
            "Enter to select · ↑/↓ to navigate · Esc to cancel"
        );
        assert_eq!(choice.options[0].number, Some(1));
        assert_eq!(choice.options[0].detail, "Crunchy red fruit");
        assert_eq!(choice.options[4].number, Some(5));
        assert!(choice.options[3].text_entry);
        assert!(!choice.options[0].text_entry);
    }

    #[test]
    fn reads_checkboxes_and_the_next_stop_of_a_multi_select() {
        let choice = pending_choice(AgentKind::Claude, panes::MULTI_NEXT).expect("menu");
        assert_eq!(
            labels(&choice),
            [
                "Red",
                "Green",
                "Blue",
                "Type something",
                "Next",
                "Chat about this"
            ]
        );
        assert_eq!(choice.selected, 4);
        assert_eq!(choice.options[4].number, None);
        assert_eq!(choice.options[0].checked, Some(false));
        assert_eq!(choice.options[0].detail, "Warm");
        assert_eq!(choice.options[4].checked, None);
        assert!(choice.options[3].text_entry);
        assert!(
            choice.prompt.ends_with("Which colors?"),
            "{}",
            choice.prompt
        );
    }

    #[test]
    fn the_menu_id_ignores_the_highlight_and_answer_marks() {
        let moved = pending_choice(AgentKind::Claude, panes::MULTI_NEXT).expect("menu");
        let toggled = panes::MULTI_NEXT
            .replace("  1. [ ] Red", "❯ 1. [✔] Red")
            .replace("❯    Next", "     Next")
            .replace("☐ Colors", "☒ Colors");
        let toggled = pending_choice(AgentKind::Claude, &toggled).expect("menu");
        assert_eq!(toggled.selected, 0);
        assert_eq!(toggled.options[0].checked, Some(true));
        assert_eq!(moved.id, toggled.id);
    }

    #[test]
    fn reads_a_permission_prompt_with_its_command() {
        let choice = pending_choice(AgentKind::Claude, panes::PERMISSION).expect("menu");
        assert_eq!(choice.options.len(), 4);
        assert_eq!(choice.options[3].label, "No");
        assert!(
            choice.options[1]
                .detail
                .starts_with("be-f2ee-4232-bf66-55238d1c251e/scratchpad")
        );
        assert!(
            choice.prompt.starts_with("Bash command\n"),
            "{}",
            choice.prompt
        );
        assert!(choice.prompt.contains("\ntouch probe-file.txt\n"));
        assert!(choice.prompt.ends_with("Do you want to proceed?"));
        assert_eq!(choice.hint, "Esc to cancel · Tab to amend");
    }

    #[test]
    fn a_different_command_is_a_different_menu() {
        let first = pending_choice(AgentKind::Claude, panes::PERMISSION).expect("menu");
        let other = panes::PERMISSION.replace(" touch probe-file.txt\n", " rm -rf build\n");
        let other = pending_choice(AgentKind::Claude, &other).expect("menu");
        assert_eq!(labels(&first), labels(&other));
        assert_ne!(first.id, other.id);
    }

    #[test]
    fn reads_plan_approval_with_the_plan_but_not_its_steps_as_options() {
        let choice = pending_choice(AgentKind::Claude, panes::PLAN).expect("menu");
        assert_eq!(
            labels(&choice),
            [
                "Yes, and use auto mode",
                "Yes, manually approve edits",
                "Tell Claude what to change"
            ]
        );
        assert!(choice.options[2].text_entry);
        assert_eq!(
            choice.options[2].detail,
            "shift+tab to approve with this feedback"
        );
        assert!(
            choice.prompt.starts_with("Ready to code?"),
            "{}",
            choice.prompt
        );
        assert!(
            choice
                .prompt
                .contains("1. Create hello.txt in the working directory.")
        );
        assert!(choice.prompt.ends_with("Would you like to proceed?"));
        assert!(choice.hint.starts_with("ctrl+g to edit in Vim"));
    }

    #[test]
    fn reads_the_review_step_that_has_no_hint_row() {
        let choice = pending_choice(AgentKind::Claude, panes::REVIEW).expect("menu");
        assert_eq!(labels(&choice), ["Submit answers", "Cancel"]);
        assert_eq!(choice.hint, "");
        assert!(choice.prompt.contains("→ Purple"), "{}", choice.prompt);
        assert!(!choice.prompt.contains("Use AskUserQuestion"));
    }

    #[test]
    fn a_typed_answer_replaces_the_placeholder() {
        let choice = pending_choice(AgentKind::Claude, panes::TYPED_ANSWER).expect("menu");
        assert_eq!(
            labels(&choice),
            ["Small", "Large", "Medium", "Chat about this"]
        );
        assert_eq!(choice.selected, 2);
        assert!(!choice.options[2].text_entry);
    }

    #[test]
    fn reads_the_unnumbered_trust_dialog() {
        let choice = pending_choice(AgentKind::Claude, panes::TRUST).expect("menu");
        assert_eq!(labels(&choice), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(choice.options[0].number, None);
        assert_eq!(choice.selected, 0);
        assert!(choice.prompt.ends_with("Security guide"));
    }

    #[test]
    fn the_composer_is_never_a_menu() {
        for pane in [panes::COMPOSER_IDLE, panes::COMPOSER_NUMBERED_DRAFT] {
            assert_eq!(pending_choice(AgentKind::Claude, pane), None, "{pane:?}");
        }
        // Without the hint row, unnumbered rows are just text.
        let bare_trust = panes::TRUST.replace(" Enter to confirm · Esc to cancel\n", "");
        assert_eq!(pending_choice(AgentKind::Claude, &bare_trust), None);
        // Text under the options means the menu is gone or isn't one.
        let trailing = format!("{}more output\nand more\nand more\n", panes::QUESTION);
        assert_eq!(pending_choice(AgentKind::Claude, &trailing), None);
    }

    /// Redraws the single-select capture the way Claude does: the highlight
    /// follows Up/Down, typing fills the text field, and Enter answers.
    struct FakePane {
        rows: Vec<String>,
        option_rows: Vec<usize>,
        selected: usize,
        keys: Vec<MenuKey>,
        answered: Option<String>,
        /// Ignores navigation keys, like a menu that stopped listening.
        frozen: bool,
        /// Replaces the question after this many key presses.
        replace_after: Option<usize>,
    }

    impl FakePane {
        fn new(selected: usize) -> Self {
            let rows = panes::QUESTION
                .replace("❯ 1. Apple", "  1. Apple")
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let option_rows = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| enumerated(row.trim_start()).is_some() && row.starts_with("  "))
                .map(|(index, _)| index)
                .collect();
            Self {
                rows,
                option_rows,
                selected,
                keys: Vec::new(),
                answered: None,
                frozen: false,
                replace_after: None,
            }
        }

        fn id(&self) -> String {
            pending_choice(AgentKind::Claude, &self.read_now())
                .expect("menu")
                .id
        }

        fn read_now(&self) -> String {
            let mut rows = self.rows.clone();
            let row = &mut rows[self.option_rows[self.selected]];
            row.replace_range(..2, "❯ ");
            rows.join("\n")
        }
    }

    impl MenuPane for FakePane {
        fn read(&mut self) -> anyhow::Result<String> {
            Ok(self.read_now())
        }

        fn press(&mut self, key: MenuKey) -> anyhow::Result<()> {
            self.keys.push(key);
            if self.replace_after == Some(self.keys.len()) {
                let question = self
                    .rows
                    .iter()
                    .position(|row| row.starts_with("Which fruit"))
                    .unwrap();
                self.rows[question] = "Which vegetable should we use?".to_owned();
            }
            match key {
                _ if self.frozen => {}
                MenuKey::Up => self.selected = self.selected.saturating_sub(1),
                MenuKey::Down => {
                    self.selected = (self.selected + 1).min(self.option_rows.len() - 1);
                }
                MenuKey::Enter => {
                    let row = &self.rows[self.option_rows[self.selected]];
                    self.answered = Some(row[5..].to_owned());
                }
                MenuKey::Escape => self.answered = Some("cancelled".to_owned()),
            }
            Ok(())
        }

        fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
            let row = &mut self.rows[self.option_rows[self.selected]];
            *row = format!("{}{text}", &row[..5]);
            Ok(())
        }

        fn pause(&mut self) {}
    }

    #[test]
    fn answering_moves_the_highlight_then_presses_enter() {
        let mut pane = FakePane::new(0);
        let menu = pane.id();
        answer(&mut pane, AgentKind::Claude, &menu, Some(2), None).expect("answered");
        assert_eq!(pane.keys, [MenuKey::Down, MenuKey::Down, MenuKey::Enter]);
        assert_eq!(pane.answered.as_deref(), Some("Cherry"));

        let mut pane = FakePane::new(3);
        answer(&mut pane, AgentKind::Claude, &menu, Some(0), None).expect("answered");
        assert_eq!(
            pane.keys,
            [MenuKey::Up, MenuKey::Up, MenuKey::Up, MenuKey::Enter]
        );
        assert_eq!(pane.answered.as_deref(), Some("Apple"));
    }

    #[test]
    fn answering_types_into_the_text_field_before_enter() {
        let mut pane = FakePane::new(0);
        let menu = pane.id();
        answer(&mut pane, AgentKind::Claude, &menu, Some(3), Some("Durian")).expect("answered");
        assert_eq!(pane.keys.last(), Some(&MenuKey::Enter));
        assert_eq!(pane.answered.as_deref(), Some("Durian"));
    }

    #[test]
    fn answering_never_presses_enter_on_a_different_menu() {
        let mut pane = FakePane::new(0);
        assert!(matches!(
            answer(
                &mut pane,
                AgentKind::Claude,
                "0000000000000000",
                Some(1),
                None
            ),
            Err(AnswerError::Stale(_))
        ));
        assert!(pane.keys.is_empty());

        let mut pane = FakePane::new(0);
        let menu = pane.id();
        pane.replace_after = Some(1);
        assert!(matches!(
            answer(&mut pane, AgentKind::Claude, &menu, Some(2), None),
            Err(AnswerError::Stale(_))
        ));
        assert_eq!(pane.keys, [MenuKey::Down]);
        assert_eq!(pane.answered, None);

        let mut pane = FakePane::new(0);
        let menu = pane.id();
        pane.frozen = true;
        assert!(matches!(
            answer(&mut pane, AgentKind::Claude, &menu, Some(2), None),
            Err(AnswerError::Stale(_))
        ));
        assert!(!pane.keys.contains(&MenuKey::Enter));
    }

    #[test]
    fn cancelling_presses_esc_only_while_the_menu_is_open() {
        let mut pane = FakePane::new(1);
        let menu = pane.id();
        answer(&mut pane, AgentKind::Claude, &menu, None, None).expect("cancelled");
        assert_eq!(pane.keys, [MenuKey::Escape]);

        // Esc on a working agent would interrupt its turn instead.
        let mut pane = FakePane::new(1);
        assert!(matches!(
            answer(&mut pane, AgentKind::Claude, "0000000000000000", None, None),
            Err(AnswerError::Stale(_))
        ));
        assert!(pane.keys.is_empty());
    }

    #[test]
    fn answering_refuses_text_the_option_cannot_take() {
        let mut pane = FakePane::new(0);
        let menu = pane.id();
        for (option, text) in [
            (1, Some("Durian")),
            (3, None),
            (3, Some("two\nlines")),
            (9, None),
        ] {
            assert!(matches!(
                answer(&mut pane, AgentKind::Claude, &menu, Some(option), text),
                Err(AnswerError::Invalid(_))
            ));
        }
        assert!(pane.keys.is_empty());
    }

    #[test]
    fn only_claude_panes_are_read() {
        assert_eq!(pending_choice(AgentKind::Codex, panes::QUESTION), None);
        assert_eq!(pending_choice(AgentKind::Other, panes::QUESTION), None);
    }
}
