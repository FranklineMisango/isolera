//! ``/etc/default/grub`` editing.
//!
//! Isolera rewrites exactly two things in the grub defaults file: the variable
//! that carries the kernel command line, and the ``GRUB_*`` variables the plan
//! pins. Everything else - comments, ordering, blank lines, variables Isolera
//! does not own - is preserved byte for byte, so the operator's diff shows only
//! the intended change.
//!
//! The file is a shell fragment, so the parser is deliberately shallow: it splits
//! lines into comments, blank lines and ``KEY=value`` assignments. A line it does
//! not understand is kept verbatim and never rewritten.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::kernel::{self, Cmdline};
use crate::sysroot::{SysRoot, WriteRecord};

/// One line of the defaults file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A blank or whitespace-only line.
    Blank(String),
    /// A comment, kept verbatim.
    Comment(String),
    /// Something Isolera does not model, kept verbatim.
    Other(String),
    /// ``KEY=value``; ``raw`` keeps the original text for an exact rewrite.
    Assignment {
        /// Variable name.
        key: String,
        /// Unquoted value, as a shell would see it.
        value: String,
        /// The line exactly as it was read (or as it should now be written).
        raw: String,
    },
}

impl Line {
    /// The text of a non-assignment line.
    fn raw(&self) -> &str {
        match self {
            Line::Blank(text) | Line::Comment(text) | Line::Other(text) => text,
            Line::Assignment { raw, .. } => raw,
        }
    }

    /// The variable name of an assignment.
    fn key(&self) -> Option<&str> {
        match self {
            Line::Assignment { key, .. } => Some(key),
            _ => None,
        }
    }
}

/// The parsed defaults file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    lines: Vec<Line>,
    /// Last assignment wins, exactly as a shell evaluating the file would see it.
    variables: BTreeMap<String, String>,
}

impl Defaults {
    /// Parse a defaults file, keeping every line for a faithful rewrite.
    pub fn parse(text: &str) -> Defaults {
        let mut lines: Vec<Line> = Vec::new();
        let mut variables: BTreeMap<String, String> = BTreeMap::new();
        for raw in text.lines() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                lines.push(Line::Blank(raw.to_string()));
            } else if trimmed.starts_with('#') {
                lines.push(Line::Comment(raw.to_string()));
            } else if let Some((key, value)) = split_assignment(trimmed) {
                variables.insert(key.to_string(), value.clone());
                lines.push(Line::Assignment {
                    key: key.to_string(),
                    value,
                    raw: raw.to_string(),
                });
            } else {
                lines.push(Line::Other(raw.to_string()));
            }
        }
        Defaults { lines, variables }
    }

    /// Read a defaults file through the sysroot (empty when it is missing).
    pub fn read(sysroot: &SysRoot, path: &str) -> Defaults {
        match sysroot.read_optional(path) {
            Some(text) => Defaults::parse(&text),
            None => Defaults::new(),
        }
    }

    /// An empty defaults file.
    pub fn new() -> Defaults {
        Defaults {
            lines: Vec::new(),
            variables: BTreeMap::new(),
        }
    }

    /// Render the file back to text, including unchanged lines byte for byte.
    ///
    /// Every line is terminated with a newline, so a file that was missing its
    /// final one is normalised rather than left in a state `git diff` flags.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line.raw());
            out.push('\n');
        }
        out
    }

    /// Value of a variable as a shell would see it.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.variables.get(key).map(String::as_str)
    }

    /// True when the file mentions the variable at all.
    pub fn contains(&self, key: &str) -> bool {
        self.variables.contains_key(key)
    }

    /// Variables in name order.
    pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Insert or replace a variable, keeping the position of the first match.
    ///
    /// Later duplicates are dropped: a shell evaluates the last assignment, so
    /// leaving one behind would silently undo the change.
    pub fn set(&mut self, key: &str, value: &str) {
        let raw = format!("{key}={}", quote_value(value));
        let mut replaced = false;
        self.lines.retain_mut(|line| match line.key() {
            Some(name) if name == key => {
                if replaced {
                    return false;
                }
                replaced = true;
                if let Line::Assignment {
                    value: existing,
                    raw: text,
                    ..
                } = line
                {
                    *existing = value.to_string();
                    *text = raw.clone();
                }
                true
            }
            _ => true,
        });
        if !replaced {
            self.lines.push(Line::Assignment {
                key: key.to_string(),
                value: value.to_string(),
                raw,
            });
        }
        self.variables.insert(key.to_string(), value.to_string());
    }

    /// The command line carried by `variable`, as a parsed map.
    pub fn cmdline(&self, variable: &str) -> Cmdline {
        Cmdline::parse(self.get(variable).unwrap_or(""))
    }

    /// Fold Isolera's parameters into the command-line variable.
    ///
    /// Returns true when the file changed, so callers can skip a no-op rewrite.
    pub fn merge_cmdline(&mut self, variable: &str, params: &Cmdline) -> bool {
        let current = self.get(variable).unwrap_or("").to_string();
        let merged = kernel::merge_managed(&current, params);
        if self.contains(variable) && merged == current {
            return false;
        }
        self.set(variable, &merged);
        true
    }
}

/// ``KEY=value`` -> ``(KEY, value)``, with quotes and shell prefix handled.
fn split_assignment(text: &str) -> Option<(&str, String)> {
    let text = match text.strip_prefix("export ") {
        Some(rest) => rest.trim_start(),
        None => text,
    };
    let (key, value) = text.split_once('=')?;
    let key = key.trim();
    if !is_shell_identifier(key) {
        return None;
    }
    Some((key, unquote(value.trim())))
}

/// ``[A-Za-z_][A-Za-z0-9_]*``, the only names the shell accepts.
pub fn is_shell_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Strip the quotes a shell would strip, and cut an unquoted inline comment.
fn unquote(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        return unescape_double(&text[1..text.len() - 1]);
    }
    if text.len() >= 2 && text.starts_with('\'') && text.ends_with('\'') {
        return text[1..text.len() - 1].to_string();
    }
    // Bare value: the shell stops at whitespace, and a `#` comment follows it.
    let cut = text
        .find(|character: char| character.is_whitespace() || character == '#')
        .unwrap_or(text.len());
    text[..cut].to_string()
}

/// Undo the escapes [`quote_value`] adds inside a double-quoted value.
fn unescape_double(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some(next @ ('\\' | '"' | '$' | '`')) => out.push(next),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Double-quote a value, escaping the characters a shell would expand.
fn quote_value(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

/// One ``KEY=value`` the plan touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableChange {
    /// Variable name.
    pub key: String,
    /// Value found in the file, when the variable was present.
    pub before: Option<String>,
    /// Value Isolera wants.
    pub after: String,
}

/// What Isolera wants ``/etc/default/grub`` to look like.
#[derive(Debug, Clone)]
pub struct GrubPlan {
    /// Logical path of the file being edited.
    pub path: String,
    /// Contents found on the node (empty when the file is missing).
    pub before: String,
    /// Intended contents.
    pub after: String,
    /// Change to the command-line variable, when it differs.
    pub cmdline: Option<VariableChange>,
    /// Changes to the variables the plan pins.
    pub variables: Vec<VariableChange>,
    /// Command the operator runs to regenerate the bootloader menu, when set.
    pub regenerate: Option<String>,
    /// True when `after` differs from `before`.
    pub changed: bool,
}

impl GrubPlan {
    /// Every change the plan makes, command line first.
    pub fn changes(&self) -> Vec<&VariableChange> {
        let mut changes: Vec<&VariableChange> = Vec::new();
        if let Some(change) = &self.cmdline {
            changes.push(change);
        }
        changes.extend(self.variables.iter());
        changes
    }
}

/// Back up the file and write the new contents.
#[derive(Debug, Clone, Default)]
pub struct GrubApply {
    /// Pre-change copy, when the file changed.
    pub backup: Option<WriteRecord>,
    /// The edit itself.
    pub record: Option<WriteRecord>,
}

impl GrubApply {
    /// Records in the order they happened.
    pub fn records(&self) -> Vec<&WriteRecord> {
        self.backup.iter().chain(self.record.iter()).collect()
    }
}

/// Work out the file Isolera wants, without touching the node.
///
/// Only real changes are reported: a variable the operator already pinned, and a
/// command line that already matches the plan, produce no change entry - which is
/// what makes a second `provision` a no-op.
pub fn plan(sysroot: &SysRoot, cfg: &Config) -> Result<GrubPlan> {
    let path = cfg.grub.file.trim();
    if path.is_empty() {
        return Err(Error::new(Kind::Config, "grub.file must not be empty"));
    }
    let variable = cfg.grub.cmdline_variable.trim();
    if !is_shell_identifier(variable) {
        return Err(Error::new(
            Kind::Config,
            format!("grub.cmdline_variable {variable:?} is not a valid shell variable name"),
        ));
    }

    let before = sysroot.read_optional(path).unwrap_or_default();
    let existing = Defaults::parse(&before);
    let mut wanted = existing.clone();

    let wanted_cmdline = existing.get(variable).map(str::to_string);
    let params = kernel::isolation_params(cfg, cfg.isolated())?;
    wanted.merge_cmdline(variable, &params);
    let merged_cmdline = wanted.get(variable).unwrap_or("").to_string();
    let cmdline = if wanted_cmdline.as_deref() == Some(merged_cmdline.as_str()) {
        None
    } else {
        Some(VariableChange {
            key: variable.to_string(),
            before: wanted_cmdline,
            after: merged_cmdline,
        })
    };

    let mut variables = Vec::new();
    for (key, value) in &cfg.grub.variables {
        if existing.get(key) == Some(value.as_str()) {
            continue;
        }
        variables.push(VariableChange {
            key: key.clone(),
            before: existing.get(key).map(str::to_string),
            after: value.clone(),
        });
        wanted.set(key, value);
    }

    let after = wanted.render();
    Ok(GrubPlan {
        path: path.to_string(),
        changed: after != before,
        before,
        after,
        cmdline,
        variables,
        regenerate: regenerate_command(cfg).map(str::to_string),
    })
}

/// Path of the pre-change copy for a given instant.
pub fn backup_path(cfg: &Config, unix_seconds: u64) -> String {
    let directory = cfg.grub.backup_dir.trim_end_matches('/');
    let stem = cfg
        .grub
        .file
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("grub");
    format!("{directory}/{stem}.{}.bak", utc_stamp(unix_seconds))
}

/// ``1970-01-01T00:00:00Z`` style stamp, derived without a calendar crate.
pub fn utc_stamp(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// Days since the Unix epoch -> civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Current wall-clock time as Unix seconds, tolerating a clock before 1970.
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Back up the file, then write the plan.
///
/// The backup is written *through* the sysroot, so it lands in the same journal as
/// the edit and `rollback` can undo both.
pub fn apply(sysroot: &SysRoot, cfg: &Config, plan: &GrubPlan) -> Result<GrubApply> {
    if !plan.changed {
        return Ok(GrubApply::default());
    }
    if !sysroot.dry_run() {
        sysroot.require_root()?;
    }

    let mut outcome = GrubApply::default();
    if !plan.before.is_empty() {
        let path = backup_path(cfg, now_seconds());
        let reason = format!("isolera: pre-change copy of {}", plan.path);
        outcome.backup = sysroot.write_text(&path, &plan.before, &reason)?;
    }
    let reason = format!("isolera: kernel command line from {}", cfg.source);
    outcome.record = sysroot.write_text(&plan.path, &plan.after, &reason)?;
    Ok(outcome)
}

/// The command that regenerates the bootloader configuration, when configured.
pub fn regenerate_command(cfg: &Config) -> Option<&str> {
    cfg.grub
        .regenerate
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Render the plan as a before/after table an operator can approve.
pub fn render_plan(plan: &GrubPlan) -> String {
    use crate::table::Table;
    if !plan.changed {
        return format!("{} is already up to date\n", plan.path);
    }
    let mut table = Table::new(["variable", "before", "after"]);
    for change in plan.changes() {
        table.push([
            change.key.clone(),
            change
                .before
                .clone()
                .unwrap_or_else(|| "(absent)".to_string()),
            crate::log::truncate(&change.after, 96),
        ]);
    }
    let mut out = format!("{}\n{}", plan.path, table.render());
    if let Some(command) = &plan.regenerate {
        out.push_str(&format!(
            "run {command} to regenerate the bootloader menu\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::fs;

    const GRUB: &str = "# Generated by isolera tests\nGRUB_DEFAULT=0\nGRUB_TIMEOUT=5\n\nGRUB_CMDLINE_LINUX=\"quiet splash\"\n";

    /// A fixture root with a grub defaults file.
    fn fixture(dry_run: bool) -> (TempDir, SysRoot) {
        let dir = TempDir::new("isolera-grub").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("etc/default")).unwrap();
        fs::write(root.join("etc/default/grub"), GRUB).unwrap();
        let sysroot = SysRoot::new(root, dry_run);
        (dir, sysroot)
    }

    /// A plan with the isolation and grub sections filled in.
    fn config(extra: &str) -> Config {
        Config::from_str_named(
            &format!(
                "[isolation]\ncpus = \"2,3\"\nhousekeeping = \"0,1\"\n\
                 [grub]\nfile = \"/etc/default/grub\"\ncmdline_variable = \"GRUB_CMDLINE_LINUX\"\n{extra}"
            ),
            "test.toml",
        )
        .unwrap()
    }

    #[test]
    fn parse_and_render_round_trip_every_line() {
        let parsed = Defaults::parse(GRUB);
        assert_eq!(parsed.render(), GRUB, "nothing is reformatted");
        assert_eq!(parsed.get("GRUB_TIMEOUT"), Some("5"));
        assert_eq!(parsed.get("GRUB_CMDLINE_LINUX"), Some("quiet splash"));
        assert_eq!(parsed.get("GRUB_MISSING"), None);
        assert!(!parsed.contains("GRUB_MISSING"));

        // A file without a trailing newline gets one, so diffs stay clean.
        assert_eq!(Defaults::parse("A=1").render(), "A=1\n");
        assert_eq!(Defaults::parse("").render(), "");
    }

    #[test]
    fn assignment_parsing_handles_shell_syntax() {
        let parsed = Defaults::parse(
            "export GRUB_TIMEOUT=5\nB='single'\nC=\"a \\\"b\\\"\"\nD=bare # trailing comment\nBROKEN LINE\n",
        );
        assert_eq!(parsed.get("GRUB_TIMEOUT"), Some("5"));
        assert_eq!(parsed.get("B"), Some("single"));
        assert_eq!(parsed.get("C"), Some("a \"b\""));
        assert_eq!(parsed.get("D"), Some("bare"));
        assert_eq!(
            parsed.get("BROKEN"),
            None,
            "a line without '=' is not an assignment"
        );
        assert!(Line::Other("BROKEN LINE".to_string()).raw() == "BROKEN LINE");
    }

    #[test]
    fn set_replaces_in_place_and_drops_duplicates() {
        let mut parsed = Defaults::parse("A=1\nB=2\nA=3\n");
        parsed.set("A", "9");
        assert_eq!(parsed.get("A"), Some("9"));
        assert_eq!(
            parsed.render(),
            "A=\"9\"\nB=2\n",
            "one assignment, first slot kept"
        );

        parsed.set("C", "new");
        assert_eq!(parsed.get("C"), Some("new"));
        assert!(parsed.render().ends_with("C=\"new\"\n"));
    }

    #[test]
    fn quoted_values_escape_shell_metacharacters() {
        let mut parsed = Defaults::parse("");
        parsed.set(
            "GRUB_CMDLINE_LINUX",
            "quiet $HOME `id` \"quoted\" back\\slash",
        );
        assert_eq!(
            parsed.render(),
            "GRUB_CMDLINE_LINUX=\"quiet \\$HOME \\`id\\` \\\"quoted\\\" back\\\\slash\"\n"
        );
        // ...and reading it back yields the original value.
        assert_eq!(
            Defaults::parse(&parsed.render()).get("GRUB_CMDLINE_LINUX"),
            Some("quiet $HOME `id` \"quoted\" back\\slash")
        );
    }

    #[test]
    fn merge_cmdline_is_in_place_and_idempotent() {
        let mut defaults = Defaults::parse(GRUB);
        let mut params = Cmdline::new();
        params.set("isolcpus", Some("domain,managed_irq,2,3"));
        params.set("nohz_full", Some("2,3"));
        assert!(defaults.merge_cmdline("GRUB_CMDLINE_LINUX", &params));
        assert_eq!(
            defaults.get("GRUB_CMDLINE_LINUX"),
            Some("quiet splash isolcpus=domain,managed_irq,2,3 nohz_full=2,3")
        );
        assert!(
            !defaults.merge_cmdline("GRUB_CMDLINE_LINUX", &params),
            "second pass is a no-op"
        );
        assert_eq!(
            defaults.get("GRUB_TIMEOUT"),
            Some("5"),
            "other variables are untouched"
        );

        // A missing variable is created.
        let mut fresh = Defaults::parse("");
        assert!(fresh.merge_cmdline("GRUB_CMDLINE_LINUX", &params));
        assert_eq!(
            fresh.get("GRUB_CMDLINE_LINUX"),
            Some("isolcpus=domain,managed_irq,2,3 nohz_full=2,3")
        );
    }

    #[test]
    fn shell_identifiers_are_checked() {
        assert!(is_shell_identifier("GRUB_CMDLINE_LINUX"));
        assert!(is_shell_identifier("_x9"));
        assert!(!is_shell_identifier("9x"));
        assert!(!is_shell_identifier("has-dash"));
        assert!(!is_shell_identifier("has space"));
        assert!(!is_shell_identifier(""));
    }

    #[test]
    fn plan_reports_only_the_changes_it_makes() {
        let (_dir, sysroot) = fixture(false);
        let cfg = config("[grub.variables]\nGRUB_TIMEOUT = \"1\"\n");
        let planned = plan(&sysroot, &cfg).unwrap();

        assert!(planned.changed);
        assert_eq!(planned.path, "/etc/default/grub");
        let cmdline = planned.cmdline.as_ref().unwrap();
        assert_eq!(cmdline.key, "GRUB_CMDLINE_LINUX");
        assert_eq!(cmdline.before.as_deref(), Some("quiet splash"));
        assert_eq!(
            cmdline.after,
            "quiet splash isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3"
        );
        assert_eq!(planned.variables.len(), 1);
        assert_eq!(planned.variables[0].key, "GRUB_TIMEOUT");
        assert_eq!(planned.variables[0].before.as_deref(), Some("5"));
        assert_eq!(planned.variables[0].after, "1");
        assert_eq!(
            planned.changes().len(),
            2,
            "command line first, then variables"
        );
        assert_eq!(planned.regenerate.as_deref(), Some("update-grub"));

        // Untouched lines survive verbatim.
        assert!(planned
            .after
            .starts_with("# Generated by isolera tests\nGRUB_DEFAULT=0\n"));
        assert!(planned.after.contains("GRUB_TIMEOUT=\"1\"\n"));
        assert!(planned.after.contains(
            "GRUB_CMDLINE_LINUX=\"quiet splash isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3\"\n"
        ));
    }

    #[test]
    fn plan_is_a_no_op_when_the_file_already_matches() {
        let (_dir, sysroot) = fixture(false);
        let cfg = config("[grub.variables]\nGRUB_TIMEOUT = \"5\"\n");
        let planned = plan(&sysroot, &cfg).unwrap();
        assert!(
            planned.cmdline.is_some(),
            "the command line still has to be merged"
        );
        assert!(planned.changed);

        // Apply, then plan again: the second plan must be empty.
        apply(&sysroot, &cfg, &planned).unwrap();
        let again = plan(&sysroot, &cfg).unwrap();
        assert!(!again.changed);
        assert!(again.cmdline.is_none());
        assert!(again.variables.is_empty());
        assert_eq!(
            render_plan(&again),
            "/etc/default/grub is already up to date\n"
        );
    }

    #[test]
    fn apply_backs_the_file_up_before_editing_it() {
        let (_dir, sysroot) = fixture(false);
        let cfg = config("[grub.variables]\nGRUB_TIMEOUT = \"1\"\n");
        let planned = plan(&sysroot, &cfg).unwrap();
        let outcome = apply(&sysroot, &cfg, &planned).unwrap();

        let backup = outcome.backup.as_ref().expect("a pre-change copy");
        assert!(
            backup.path.starts_with("/var/lib/isolera/backups/grub."),
            "{}",
            backup.path
        );
        assert!(backup.path.ends_with(".bak"));
        assert_eq!(
            sysroot.read_optional(&backup.path).unwrap(),
            GRUB,
            "backup keeps the original"
        );
        assert_eq!(
            sysroot.read_optional("/etc/default/grub").unwrap(),
            planned.after
        );

        let record = outcome.record.as_ref().unwrap();
        assert!(!record.created, "the file already existed");
        assert!(record.reversible);
        assert_eq!(outcome.records().len(), 2, "backup first, then the edit");
    }

    #[test]
    fn apply_creates_a_missing_file_without_a_backup() {
        let dir = TempDir::new("isolera-grub-new").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("etc")).unwrap();
        let sysroot = SysRoot::new(root, false);
        let cfg = config("");
        let planned = plan(&sysroot, &cfg).unwrap();
        assert_eq!(planned.before, "");
        assert!(planned.changed);
        assert!(planned.cmdline.as_ref().unwrap().before.is_none());

        let outcome = apply(&sysroot, &cfg, &planned).unwrap();
        assert!(outcome.backup.is_none(), "nothing to copy");
        assert!(outcome.record.as_ref().unwrap().created);
        assert_eq!(
            sysroot.read_optional("/etc/default/grub").unwrap(),
            planned.after
        );
    }

    #[test]
    fn dry_run_never_touches_the_file() {
        let (_dir, sysroot) = fixture(true);
        let cfg = config("[grub.variables]\nGRUB_TIMEOUT = \"1\"\n");
        let planned = plan(&sysroot, &cfg).unwrap();
        let outcome = apply(&sysroot, &cfg, &planned).unwrap();
        assert_eq!(outcome.records().len(), 2);
        assert!(outcome.records().iter().all(|record| !record.applied));
        assert_eq!(sysroot.read_optional("/etc/default/grub").unwrap(), GRUB);
    }

    #[test]
    fn plan_rejects_an_unusable_configuration() {
        let (_dir, sysroot) = fixture(false);
        let mut broken = config("");
        broken.grub.cmdline_variable = "not valid".to_string();
        let err = plan(&sysroot, &broken).unwrap_err();
        assert_eq!(err.kind(), Kind::Config);
        assert!(
            err.message().contains("shell variable"),
            "{}",
            err.message()
        );

        let mut empty = config("");
        empty.grub.file = "   ".to_string();
        assert!(plan(&sysroot, &empty).is_err());
    }

    #[test]
    fn backup_paths_and_stamps_are_predictable() {
        let cfg = config("");
        assert_eq!(
            backup_path(&cfg, 0),
            "/var/lib/isolera/backups/grub.1970-01-01T00:00:00Z.bak"
        );
        assert_eq!(utc_stamp(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(utc_stamp(1_709_000_000), "2024-02-27T02:13:20Z");
        assert_eq!(utc_stamp(1_735_689_600), "2025-01-01T00:00:00Z");
        assert!(now_seconds() > 1_700_000_000, "the clock is sane");
        assert_eq!(regenerate_command(&cfg), Some("update-grub"));
    }

    #[test]
    fn render_plan_shows_the_approval_table() {
        let (_dir, sysroot) = fixture(false);
        let cfg = config("[grub.variables]\nGRUB_TIMEOUT = \"1\"\n");
        let rendered = render_plan(&plan(&sysroot, &cfg).unwrap());
        assert!(
            rendered.starts_with("/etc/default/grub\nvariable"),
            "{rendered}"
        );
        assert!(rendered.contains("GRUB_CMDLINE_LINUX"));
        assert!(rendered.contains("quiet splash"));
        assert!(rendered.contains("GRUB_TIMEOUT"));
        assert!(rendered.contains("run update-grub to regenerate the bootloader menu"));

        // A plan with no follow-up command says nothing about regenerating.
        let quiet = render_plan(&plan(&sysroot, &config("regenerate = \"none\"\n")).unwrap());
        assert!(!quiet.contains("regenerate"), "{quiet}");
    }
}
