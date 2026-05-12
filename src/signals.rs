use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RuleScope {
    Always,
    Dnd,
}

impl fmt::Display for RuleScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleScope::Always => f.write_str("always"),
            RuleScope::Dnd => f.write_str("dnd"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignalRuleKind {
    App,
    Website,
}

impl fmt::Display for SignalRuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalRuleKind::App => f.write_str("app"),
            SignalRuleKind::Website => f.write_str("website"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SignalRule {
    pub kind: SignalRuleKind,
    pub scope: RuleScope,
    pub pattern: String,
}

impl SignalRule {
    pub fn new(kind: SignalRuleKind, scope: RuleScope, pattern: String) -> Result<Self> {
        let pattern = pattern.trim().to_string();
        if pattern.is_empty() {
            bail!("rule pattern cannot be empty");
        }
        Ok(Self {
            kind,
            scope,
            pattern,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SignalConfig {
    #[serde(default = "default_version")]
    pub version: u8,
    #[serde(default)]
    pub rules: Vec<SignalRule>,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            rules: vec![
                SignalRule {
                    kind: SignalRuleKind::App,
                    scope: RuleScope::Dnd,
                    pattern: "Messages".to_string(),
                },
                SignalRule {
                    kind: SignalRuleKind::App,
                    scope: RuleScope::Dnd,
                    pattern: "Outlook".to_string(),
                },
                SignalRule {
                    kind: SignalRuleKind::Website,
                    scope: RuleScope::Always,
                    pattern: "youtube".to_string(),
                },
            ],
        }
    }
}

impl SignalConfig {
    pub fn path(explicit: Option<&str>) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set; pass --rules-file"))?;
        Ok(home.join(".config/pavlov/rules.json"))
    }

    pub fn load(explicit_path: Option<&str>) -> Result<Self> {
        let path = Self::path(explicit_path)?;
        match fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Ok(Self::default()),
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parse signal rules from {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, explicit_path: Option<&str>) -> Result<()> {
        let path = Self::path(explicit_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let contents = serde_json::to_string_pretty(self).context("serialize signal rules")?;
        fs::write(&path, format!("{contents}\n"))
            .with_context(|| format!("write {}", path.display()))
    }

    pub fn add_rule(&mut self, rule: SignalRule) -> bool {
        if self.rules.iter().any(|existing| same_rule(existing, &rule)) {
            return false;
        }
        self.rules.push(rule);
        true
    }

    pub fn remove_rule(
        &mut self,
        kind: SignalRuleKind,
        scope: Option<RuleScope>,
        pattern: &str,
    ) -> usize {
        let before = self.rules.len();
        self.rules.retain(|rule| {
            rule.kind != kind
                || scope.is_some_and(|scope| rule.scope != scope)
                || !eq_folded(rule.pattern.as_str(), pattern)
        });
        before - self.rules.len()
    }

    pub fn violations<'a>(&'a self, snapshot: &'a SignalSnapshot) -> Vec<SignalViolation<'a>> {
        self.rules
            .iter()
            .filter(|rule| rule.scope == RuleScope::Always || snapshot.dnd_enabled)
            .filter(|rule| match rule.kind {
                SignalRuleKind::App => snapshot
                    .frontmost_app
                    .as_deref()
                    .is_some_and(|app| contains_folded(app, &rule.pattern)),
                SignalRuleKind::Website => {
                    snapshot
                        .browser_url
                        .as_deref()
                        .is_some_and(|url| contains_folded(url, &rule.pattern))
                        || snapshot
                            .browser_title
                            .as_deref()
                            .is_some_and(|title| contains_folded(title, &rule.pattern))
                        || snapshot.open_browser_tabs.iter().any(|tab| {
                            tab.url
                                .as_deref()
                                .is_some_and(|url| contains_folded(url, &rule.pattern))
                                || tab
                                    .title
                                    .as_deref()
                                    .is_some_and(|title| contains_folded(title, &rule.pattern))
                        })
                }
            })
            .map(|rule| SignalViolation { rule })
            .collect()
    }
}

pub struct SignalViolation<'a> {
    pub rule: &'a SignalRule,
}

impl SignalViolation<'_> {
    pub fn cooldown_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.rule.kind,
            self.rule.scope,
            self.rule.pattern.to_ascii_lowercase()
        )
    }

    pub fn signal_name(&self) -> &'static str {
        match (self.rule.kind, self.rule.scope) {
            (SignalRuleKind::App, RuleScope::Always) => "disallowed_app",
            (SignalRuleKind::App, RuleScope::Dnd) => "disallowed_app_during_dnd",
            (SignalRuleKind::Website, RuleScope::Always) => "disallowed_website",
            (SignalRuleKind::Website, RuleScope::Dnd) => "disallowed_website_during_dnd",
        }
    }
}

#[derive(Debug, Default)]
pub struct SignalSnapshot {
    pub frontmost_app: Option<String>,
    pub browser_url: Option<String>,
    pub browser_title: Option<String>,
    pub open_browser_tabs: Vec<BrowserTab>,
    pub dnd_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BrowserTab {
    pub app: String,
    pub url: Option<String>,
    pub title: Option<String>,
}

pub struct SignalCooldowns {
    cooldown: Duration,
    last_fired: std::collections::HashMap<String, Instant>,
}

impl SignalCooldowns {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_fired: std::collections::HashMap::new(),
        }
    }

    pub fn ready(&mut self, key: String, now: Instant) -> bool {
        if self
            .last_fired
            .get(&key)
            .is_some_and(|last| now.duration_since(*last) < self.cooldown)
        {
            return false;
        }
        self.last_fired.insert(key, now);
        true
    }
}

pub fn read_snapshot(dnd_command: Option<&str>) -> (SignalSnapshot, Vec<String>) {
    let mut warnings = Vec::new();
    let frontmost_app = match read_frontmost_app() {
        Ok(app) => app,
        Err(error) => {
            warnings.push(format!("frontmost app detection failed: {error}"));
            None
        }
    };

    let browser = frontmost_app
        .as_deref()
        .and_then(|app| match read_browser_state(app) {
            Ok(browser) => browser,
            Err(error) => {
                warnings.push(format!("browser state detection failed for {app}: {error}"));
                None
            }
        });
    let open_browser_tabs = match read_open_browser_tabs() {
        Ok(tabs) => tabs,
        Err(error) => {
            warnings.push(format!("open browser tab detection failed: {error}"));
            Vec::new()
        }
    };

    let dnd_enabled = match read_dnd_enabled(dnd_command) {
        Ok(dnd_enabled) => dnd_enabled,
        Err(error) => {
            warnings.push(format!(
                "Do Not Disturb detection failed: {error}. DND-scoped rules will not fire unless you pass --dnd-command."
            ));
            false
        }
    };

    (
        SignalSnapshot {
            frontmost_app,
            browser_url: browser.as_ref().and_then(|browser| browser.url.clone()),
            browser_title: browser.and_then(|browser| browser.title),
            open_browser_tabs,
            dnd_enabled,
        },
        warnings,
    )
}

struct BrowserState {
    url: Option<String>,
    title: Option<String>,
}

fn read_frontmost_app() -> Result<Option<String>> {
    let script = r#"tell application "System Events" to get name of first application process whose frontmost is true"#;
    match run_osascript(script).context("run System Events AppleScript") {
        Ok(app) => Ok(non_empty(app)),
        Err(error) => read_frontmost_app_via_lsappinfo().or(Err(error)),
    }
}

fn read_frontmost_app_via_lsappinfo() -> Result<Option<String>> {
    let front = run_command("/usr/bin/lsappinfo", &["front"])?;
    let Some(asn) = front
        .split_whitespace()
        .find(|part| part.starts_with("ASN:"))
        .map(|part| part.trim_end_matches(':').to_string() + ":")
    else {
        return Ok(None);
    };

    let info = run_command("/usr/bin/lsappinfo", &["info", &asn])?;
    Ok(info
        .lines()
        .next()
        .and_then(|line| line.split('"').nth(1))
        .map(str::to_string))
}

fn read_browser_state(app: &str) -> Result<Option<BrowserState>> {
    let Some(family) = browser_family(app) else {
        return Ok(None);
    };
    let script = match family {
        BrowserFamily::Safari => format!(
            r#"tell application "{}"
if not (exists front window) then return ""
set theUrl to URL of current tab of front window
set theTitle to name of current tab of front window
return theUrl & linefeed & theTitle
end tell"#,
            escape_osascript_string(app)
        ),
        BrowserFamily::Chromium => format!(
            r#"tell application "{}"
if not (exists front window) then return ""
set theUrl to URL of active tab of front window
set theTitle to title of active tab of front window
return theUrl & linefeed & theTitle
end tell"#,
            escape_osascript_string(app)
        ),
        BrowserFamily::WindowTitleOnly => format!(
            r#"tell application "System Events" to tell process "{}"
if not (exists front window) then return ""
return name of front window
end tell"#,
            escape_osascript_string(app)
        ),
    };
    let output = run_osascript(&script)?;
    let mut lines = output.lines();
    match family {
        BrowserFamily::WindowTitleOnly => Ok(Some(BrowserState {
            url: None,
            title: non_empty(output),
        })),
        _ => Ok(Some(BrowserState {
            url: lines.next().and_then(|line| non_empty(line.to_string())),
            title: lines.next().and_then(|line| non_empty(line.to_string())),
        })),
    }
}

fn read_open_browser_tabs() -> Result<Vec<BrowserTab>> {
    let mut tabs = Vec::new();
    for app in [
        "Google Chrome",
        "Chromium",
        "Brave Browser",
        "Microsoft Edge",
        "Arc",
        "Dia",
        "Opera",
        "Safari",
        "Firefox",
    ] {
        let Some(family) = browser_family(app) else {
            continue;
        };
        if !app_is_running(app) {
            continue;
        }
        if let Ok(app_tabs) = read_browser_tabs_for_app(app, family) {
            tabs.extend(app_tabs);
        }
    }
    Ok(tabs)
}

fn read_browser_tabs_for_app(app: &str, family: BrowserFamily) -> Result<Vec<BrowserTab>> {
    let script = match family {
        BrowserFamily::Safari => format!(
            r#"tell application "{}"
set output to ""
repeat with windowRef in windows
repeat with tabRef in tabs of windowRef
set output to output & (URL of tabRef as text) & tab & (name of tabRef as text) & linefeed
end repeat
end repeat
return output
end tell"#,
            escape_osascript_string(app)
        ),
        BrowserFamily::Chromium => format!(
            r#"tell application "{}"
set output to ""
repeat with windowRef in windows
repeat with tabRef in tabs of windowRef
set output to output & (URL of tabRef as text) & tab & (title of tabRef as text) & linefeed
end repeat
end repeat
return output
end tell"#,
            escape_osascript_string(app)
        ),
        BrowserFamily::WindowTitleOnly => format!(
            r#"tell application "System Events" to tell process "{}"
set output to ""
repeat with windowRef in windows
set output to output & (name of windowRef as text) & linefeed
end repeat
return output
end tell"#,
            escape_osascript_string(app)
        ),
    };
    let output = run_osascript(&script)?;
    Ok(parse_browser_tabs(app, family, &output))
}

fn parse_browser_tabs(app: &str, family: BrowserFamily, output: &str) -> Vec<BrowserTab> {
    output
        .lines()
        .filter_map(|line| {
            if family == BrowserFamily::WindowTitleOnly {
                return non_empty(line.to_string()).map(|title| BrowserTab {
                    app: app.to_string(),
                    url: None,
                    title: Some(title),
                });
            }
            let (url, title) = line.split_once('\t').unwrap_or((line, ""));
            let url = non_empty(url.to_string());
            let title = non_empty(title.to_string());
            (url.is_some() || title.is_some()).then(|| BrowserTab {
                app: app.to_string(),
                url,
                title,
            })
        })
        .collect()
}

fn app_is_running(app: &str) -> bool {
    run_command("/usr/bin/pgrep", &["-x", app]).is_ok()
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BrowserFamily {
    Safari,
    Chromium,
    WindowTitleOnly,
}

fn browser_family(app: &str) -> Option<BrowserFamily> {
    let app = app.to_ascii_lowercase();
    if app.contains("safari") {
        Some(BrowserFamily::Safari)
    } else if app.contains("chrome")
        || app.contains("chromium")
        || app.contains("brave")
        || app.contains("edge")
        || app == "arc"
        || app.contains("opera")
        || app == "dia"
    {
        Some(BrowserFamily::Chromium)
    } else if app.contains("firefox") {
        Some(BrowserFamily::WindowTitleOnly)
    } else {
        None
    }
}

fn read_dnd_enabled(dnd_command: Option<&str>) -> Result<bool> {
    if let Some(command) = dnd_command {
        return read_dnd_from_command(command);
    }

    for args in [
        &[
            "-currentHost",
            "read",
            "com.apple.notificationcenterui",
            "doNotDisturb",
        ][..],
        &["read", "com.apple.notificationcenterui", "doNotDisturb"][..],
    ] {
        if let Ok(output) = run_command("/usr/bin/defaults", &args) {
            if let Some(value) = parse_boolish(&output) {
                return Ok(value);
            }
        }
    }

    if let Some(value) = read_dnd_from_menu_bar()? {
        return Ok(value);
    }

    bail!("macOS does not expose a stable Focus status API, and no readable fallback worked")
}

fn read_dnd_from_command(command: &str) -> Result<bool> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .output()
        .with_context(|| format!("run DND command {command:?}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if let Some(value) = parse_boolish(&stdout) {
        return Ok(value);
    }
    if stdout.is_empty() && output.status.success() {
        return Ok(true);
    }
    if stdout.is_empty() && !output.status.success() {
        return Ok(false);
    }
    bail!(
        "DND command printed an unrecognized value {stdout:?}{}",
        if stderr.is_empty() {
            String::new()
        } else {
            format!(" ({stderr})")
        }
    )
}

fn read_dnd_from_menu_bar() -> Result<Option<bool>> {
    let script = r#"tell application "System Events"
if not (exists process "ControlCenter") then return "unknown"
tell process "ControlCenter"
repeat with itemRef in menu bar items of menu bar 1
set bits to ""
try
set bits to bits & (description of itemRef as text) & linefeed
end try
try
set bits to bits & (value of itemRef as text) & linefeed
end try
try
set bits to bits & (title of itemRef as text) & linefeed
end try
if bits contains "Focus" or bits contains "Do Not Disturb" then return bits
end repeat
end tell
end tell
return "unknown""#;
    let output = run_osascript(script).context("read Focus menu bar item")?;
    let output = output.trim();
    if output.eq_ignore_ascii_case("unknown") || output.is_empty() {
        return Ok(None);
    }
    if contains_folded(output, "do not disturb") || contains_folded(output, "focus") {
        return Ok(Some(
            !contains_folded(output, "off") && !contains_folded(output, "inactive"),
        ));
    }
    Ok(None)
}

fn run_osascript(script: &str) -> Result<String> {
    run_osascript_with_timeout(script, Duration::from_secs(2))
}

fn run_osascript_with_timeout(script: &str, duration: Duration) -> Result<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run osascript")?;
    let started = Instant::now();
    loop {
        if child.try_wait().context("poll osascript")?.is_some() {
            break;
        }
        if started.elapsed() >= duration {
            child.kill().ok();
            let output = child
                .wait_with_output()
                .context("wait for killed osascript")?;
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "osascript timed out{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().context("wait for osascript")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_boolish(value: &str) -> Option<bool> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" | "enabled" | "active" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" | "inactive" => Some(false),
        _ => None,
    }
}

fn same_rule(a: &SignalRule, b: &SignalRule) -> bool {
    a.kind == b.kind && a.scope == b.scope && eq_folded(&a.pattern, &b.pattern)
}

fn contains_folded(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn eq_folded(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn escape_osascript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn default_version() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_requested_signals() {
        let config = SignalConfig::default();
        let rules = config
            .rules
            .iter()
            .map(|rule| (rule.kind, rule.scope, rule.pattern.as_str()))
            .collect::<Vec<_>>();

        assert!(rules.contains(&(SignalRuleKind::App, RuleScope::Dnd, "Messages")));
        assert!(rules.contains(&(SignalRuleKind::App, RuleScope::Dnd, "Outlook")));
        assert!(rules.contains(&(SignalRuleKind::Website, RuleScope::Always, "youtube")));
    }

    #[test]
    fn detects_dnd_app_and_always_site_violations() {
        let config = SignalConfig::default();

        let quiet_messages = SignalSnapshot {
            frontmost_app: Some("Messages".to_string()),
            dnd_enabled: true,
            ..SignalSnapshot::default()
        };
        assert_eq!(config.violations(&quiet_messages).len(), 1);

        let normal_messages = SignalSnapshot {
            frontmost_app: Some("Messages".to_string()),
            dnd_enabled: false,
            ..SignalSnapshot::default()
        };
        assert!(config.violations(&normal_messages).is_empty());

        let youtube = SignalSnapshot {
            frontmost_app: Some("Google Chrome".to_string()),
            browser_url: Some("https://www.youtube.com/watch?v=abc".to_string()),
            dnd_enabled: false,
            ..SignalSnapshot::default()
        };
        assert_eq!(config.violations(&youtube).len(), 1);
    }

    #[test]
    fn add_and_remove_rules_are_case_insensitive() {
        let mut config = SignalConfig::default();
        assert!(
            !config.add_rule(
                SignalRule::new(
                    SignalRuleKind::Website,
                    RuleScope::Always,
                    "YouTube".to_string()
                )
                .expect("rule")
            )
        );
        assert!(
            config.add_rule(
                SignalRule::new(
                    SignalRuleKind::Website,
                    RuleScope::Dnd,
                    "YouTube".to_string()
                )
                .expect("rule")
            )
        );

        assert_eq!(
            config.remove_rule(SignalRuleKind::Website, Some(RuleScope::Dnd), "youtube"),
            1
        );
    }

    #[test]
    fn cooldown_tracks_each_rule_key() {
        let mut cooldowns = SignalCooldowns::new(Duration::from_secs(30));
        let now = Instant::now();

        assert!(cooldowns.ready("a".to_string(), now));
        assert!(!cooldowns.ready("a".to_string(), now + Duration::from_secs(1)));
        assert!(cooldowns.ready("b".to_string(), now + Duration::from_secs(1)));
        assert!(cooldowns.ready("a".to_string(), now + Duration::from_secs(31)));
    }
}
