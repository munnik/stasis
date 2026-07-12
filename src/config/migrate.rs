// Author: Dustin Pilgrim
// License: GPL-3.0-only

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum MigrateOutcome {
    NotOldFormat,
    Migrated { backup_path: PathBuf },
}

pub fn looks_like_old_config(text: &str) -> bool {
    // Old shipped config has `stasis:` and usually `profiles:`
    // New config has `default:`
    let has_stasis = text.lines().any(|l| l.trim_start().starts_with("stasis:"));
    let has_default = text.lines().any(|l| l.trim_start().starts_with("default:"));
    has_stasis && !has_default
}

fn looks_like_new_with_use_loginctl(text: &str) -> bool {
    let has_default = text.lines().any(|l| l.trim_start().starts_with("default:"));
    let has_use = text
        .lines()
        .any(|l| l.trim_start().starts_with("use_loginctl "));
    has_default && has_use
}

fn looks_like_new_with_legacy_loginctl_key(text: &str) -> bool {
    let has_default = text.lines().any(|l| l.trim_start().starts_with("default:"));
    let has_legacy_key = text
        .lines()
        .any(|l| l.trim_start().starts_with("enable_loginctl "));
    has_default && has_legacy_key
}

fn looks_like_new_with_legacy_dbus_inhibit_key(text: &str) -> bool {
    let has_default = text.lines().any(|l| l.trim_start().starts_with("default:"));
    let has_legacy_key = text
        .lines()
        .any(|l| l.trim_start().starts_with("listen_browser_dbus_inhibit "));
    has_default && has_legacy_key
}

pub fn migrate_in_place(path: &Path) -> Result<MigrateOutcome, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;

    // New-format rewrites:
    // - hoist per-step `use_loginctl` into default.enable_loginctl_integration
    // - rename `enable_loginctl` -> `enable_loginctl_integration`
    // - rename legacy `listen_browser_dbus_inhibit` -> `enable_dbus_inhibit`
    if looks_like_new_with_use_loginctl(&text)
        || looks_like_new_with_legacy_dbus_inhibit_key(&text)
        || looks_like_new_with_legacy_loginctl_key(&text)
    {
        let new_text = migrate_new_keys(&text);

        let backup_path = backup_name(path);
        let _ = fs::remove_file(&backup_path);

        fs::rename(path, &backup_path).map_err(|e| {
            format!(
                "backup rename {} -> {}: {e}",
                path.display(),
                backup_path.display()
            )
        })?;

        fs::write(path, new_text).map_err(|e| format!("write new {}: {e}", path.display()))?;

        return Ok(MigrateOutcome::Migrated { backup_path });
    }

    // Old-format migration.
    if !looks_like_old_config(&text) {
        return Ok(MigrateOutcome::NotOldFormat);
    }

    let old = parse_old(&text).map_err(|e| format!("parse old config: {e}"))?;
    let new_text = emit_new(&old);

    let backup_path = backup_name(path);

    // Option A: overwrite any existing .bak
    let _ = fs::remove_file(&backup_path);

    fs::rename(path, &backup_path).map_err(|e| {
        format!(
            "backup rename {} -> {}: {e}",
            path.display(),
            backup_path.display()
        )
    })?;

    fs::write(path, new_text).map_err(|e| format!("write new {}: {e}", path.display()))?;

    Ok(MigrateOutcome::Migrated { backup_path })
}

fn backup_name(path: &Path) -> PathBuf {
    // "<original>.bak" in the same directory
    PathBuf::from(format!("{}.bak", path.display()))
}

/* ---------------- new-with-use_loginctl rewrite ---------------- */

fn migrate_new_keys(text: &str) -> String {
    // Drop all `use_loginctl ...` lines, remember if any were true.
    let mut saw_true = false;
    let mut out_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("listen_browser_dbus_inhibit ") {
            let indent = line
                .chars()
                .take_while(|c| c.is_ascii_whitespace())
                .collect::<String>();
            let val = t.split_whitespace().nth(1).unwrap_or("true");
            out_lines.push(format!("{indent}enable_dbus_inhibit {val}"));
            continue;
        }
        if t.starts_with("enable_loginctl ") {
            let indent = line
                .chars()
                .take_while(|c| c.is_ascii_whitespace())
                .collect::<String>();
            let val = t.split_whitespace().nth(1).unwrap_or("true");
            out_lines.push(format!("{indent}enable_loginctl_integration {val}"));
            continue;
        }
        if t.starts_with("use_loginctl ") {
            if t.split_whitespace()
                .nth(1)
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
            {
                saw_true = true;
            }
            continue; // drop it
        }

        out_lines.push(line.to_string());
    }

    // If we did not need to insert enable_loginctl_integration, return the rewritten text.
    if !saw_true {
        return ensure_trailing_newline(out_lines.join("\n"));
    }

    // If enable_loginctl_integration already exists anywhere, don't insert another.
    let has_enable = out_lines
        .iter()
        .any(|l| l.trim_start().starts_with("enable_loginctl_integration "));
    if has_enable {
        return ensure_trailing_newline(out_lines.join("\n"));
    }

    // Insert under `default:`
    let mut rewritten: Vec<String> = Vec::new();
    let mut inserted = false;

    for l in out_lines {
        rewritten.push(l.clone());
        if !inserted && l.trim_start() == "default:" {
            rewritten.push("  enable_loginctl_integration true".to_string());
            inserted = true;
        }
    }

    ensure_trailing_newline(rewritten.join("\n"))
}

fn ensure_trailing_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/* ---------------- old model ---------------- */

#[derive(Debug, Default, Clone)]
struct OldFile {
    meta_lines: Vec<String>, // @author, @description, etc.
    globals: Vec<Line>,      // top-level things (rare)
    stasis: OldStasis,
    profiles: Vec<OldProfile>,
}

#[derive(Debug, Default, Clone)]
struct OldStasis {
    globals: Vec<Line>,
    blocks: Vec<OldBlock>, // desktop blocks inside stasis:
    on_ac: Vec<OldBlock>,
    on_battery: Vec<OldBlock>,
}

#[derive(Debug, Default, Clone)]
struct OldProfile {
    name: String,
    lines: Vec<Line>,      // globals inside profile
    blocks: Vec<OldBlock>, // blocks inside profile
}

#[derive(Debug, Default, Clone)]
struct OldBlock {
    name: String,     // lock_screen, dpms, custom-foo, etc.
    lines: Vec<Line>, // properties inside block
}

#[derive(Debug, Clone)]
struct Line {
    key: String,
    raw_value: String, // preserve, we’ll rewrite some keys
}

/* ---------------- parsing ---------------- */

fn parse_old(text: &str) -> Result<OldFile, String> {
    // Very small block parser with `end` terminators.
    // We treat indentation as cosmetic; block nesting is driven by `:` and `end`.
    let mut f = OldFile::default();

    let mut ctx: Vec<Ctx> = Vec::new();

    for original in text.lines() {
        let line = original.trim();
        if line.is_empty() || line.starts_with('#') {
            // Keep metadata-ish comments only if they are @ lines; others are dropped in v1
            if line.starts_with('@') {
                f.meta_lines.push(original.to_string());
            }
            continue;
        }
        if line.starts_with('@') {
            f.meta_lines.push(original.to_string());
            continue;
        }

        if line == "end" {
            ctx.pop();
            continue;
        }

        // block start?
        if line.ends_with(':') {
            let name = line.trim_end_matches(':').trim().to_string();
            // top-level stasis / profiles / default etc
            match (ctx.last(), name.as_str()) {
                (None, "stasis") => ctx.push(Ctx::Stasis),
                (None, "profiles") => ctx.push(Ctx::Profiles),
                (Some(Ctx::Stasis), "on_ac") => ctx.push(Ctx::OnAc),
                (Some(Ctx::Stasis), "on_battery") => ctx.push(Ctx::OnBattery),
                (Some(Ctx::Profiles), profile_name) => {
                    f.profiles.push(OldProfile {
                        name: profile_name.to_string(),
                        ..Default::default()
                    });
                    ctx.push(Ctx::Profile);
                }
                (Some(Ctx::Stasis), block_name) => {
                    f.stasis.blocks.push(OldBlock {
                        name: block_name.to_string(),
                        ..Default::default()
                    });
                    ctx.push(Ctx::Block {
                        where_: BlockWhere::StasisDesktop,
                    });
                }
                (Some(Ctx::OnAc), block_name) => {
                    f.stasis.on_ac.push(OldBlock {
                        name: block_name.to_string(),
                        ..Default::default()
                    });
                    ctx.push(Ctx::Block {
                        where_: BlockWhere::StasisAc,
                    });
                }
                (Some(Ctx::OnBattery), block_name) => {
                    f.stasis.on_battery.push(OldBlock {
                        name: block_name.to_string(),
                        ..Default::default()
                    });
                    ctx.push(Ctx::Block {
                        where_: BlockWhere::StasisBattery,
                    });
                }
                (Some(Ctx::Profile), block_name) => {
                    let p = f.profiles.last_mut().ok_or("profile context missing")?;
                    p.blocks.push(OldBlock {
                        name: block_name.to_string(),
                        ..Default::default()
                    });
                    ctx.push(Ctx::Block {
                        where_: BlockWhere::Profile,
                    });
                }
                _ => {
                    // unknown nesting; ignore
                }
            }
            continue;
        }

        // key/value line OR array (we store raw)
        let (k, v) = split_kv(line).ok_or_else(|| format!("cannot parse line: {line}"))?;
        let cur = ctx.last().cloned();

        match cur {
            Some(Ctx::Stasis) => f.stasis.globals.push(Line {
                key: k,
                raw_value: v,
            }),
            Some(Ctx::Profiles) => f.globals.push(Line {
                key: k,
                raw_value: v,
            }),
            Some(Ctx::Profile) => {
                let p = f.profiles.last_mut().ok_or("profile missing")?;
                p.lines.push(Line {
                    key: k,
                    raw_value: v,
                });
            }
            Some(Ctx::OnAc) => f.stasis.globals.push(Line {
                key: k,
                raw_value: v,
            }), // ignore
            Some(Ctx::OnBattery) => f.stasis.globals.push(Line {
                key: k,
                raw_value: v,
            }), // ignore
            Some(Ctx::Block { where_ }) => match where_ {
                BlockWhere::StasisDesktop => {
                    let b = f.stasis.blocks.last_mut().ok_or("block missing")?;
                    b.lines.push(Line {
                        key: k,
                        raw_value: v,
                    });
                }
                BlockWhere::StasisAc => {
                    let b = f.stasis.on_ac.last_mut().ok_or("block missing")?;
                    b.lines.push(Line {
                        key: k,
                        raw_value: v,
                    });
                }
                BlockWhere::StasisBattery => {
                    let b = f.stasis.on_battery.last_mut().ok_or("block missing")?;
                    b.lines.push(Line {
                        key: k,
                        raw_value: v,
                    });
                }
                BlockWhere::Profile => {
                    let p = f.profiles.last_mut().ok_or("profile missing")?;
                    let b = p.blocks.last_mut().ok_or("profile block missing")?;
                    b.lines.push(Line {
                        key: k,
                        raw_value: v,
                    });
                }
            },
            None => {
                // top-level non-block kv; keep as meta-ish
                f.globals.push(Line {
                    key: k,
                    raw_value: v,
                });
            }
        }
    }

    Ok(f)
}

#[derive(Debug, Clone)]
enum Ctx {
    Stasis,
    Profiles,
    Profile,
    OnAc,
    OnBattery,
    Block { where_: BlockWhere },
}

#[derive(Debug, Clone)]
enum BlockWhere {
    StasisDesktop,
    StasisAc,
    StasisBattery,
    Profile,
}

fn split_kv(line: &str) -> Option<(String, String)> {
    // Accept:
    //   key value
    //   key "string"
    //   key [ ... ]
    // No colon here (handled earlier)
    let mut it = line.splitn(2, char::is_whitespace);
    let k = it.next()?.trim().to_string();
    let rest = it.next()?.trim().to_string();
    Some((normalize_key(&k), rest))
}

fn normalize_key(k: &str) -> String {
    k.trim().replace('-', "_")
}

/* ---------------- emitting ---------------- */

fn old_wants_loginctl(old: &OldFile) -> bool {
    fn block_wants_loginctl(b: &OldBlock) -> bool {
        let name = b.name.trim().replace('-', "_");
        if name != "lock_screen" {
            return false;
        }

        for l in &b.lines {
            if l.key == "use_loginctl"
                && l.raw_value.trim().split_whitespace().next() == Some("true")
            {
                return true;
            }
            if l.key == "command" && l.raw_value.contains("loginctl lock-session") {
                return true;
            }
        }
        false
    }

    old.stasis.blocks.iter().any(block_wants_loginctl)
        || old.stasis.on_ac.iter().any(block_wants_loginctl)
        || old.stasis.on_battery.iter().any(block_wants_loginctl)
        || old
            .profiles
            .iter()
            .flat_map(|p| p.blocks.iter())
            .any(block_wants_loginctl)
}

fn emit_new(old: &OldFile) -> String {
    let mut out = String::new();

    // Metadata lines (keep @author/@description)
    for m in &old.meta_lines {
        out.push_str(m);
        out.push('\n');
    }
    if !old.meta_lines.is_empty() {
        out.push('\n');
    }

    // DEFAULT
    out.push_str("default:\n");

    // If old config implied loginctl mode, hoist it to global.
    let want_loginctl = old_wants_loginctl(old);
    let has_enable_already = old
        .stasis
        .globals
        .iter()
        .any(|l| l.key == "enable_loginctl_integration");

    if want_loginctl && !has_enable_already {
        out.push_str("  enable_loginctl_integration true\n");
    }

    emit_globals(&mut out, &old.stasis.globals, 2);

    // Desktop blocks (inside default)
    for b in &old.stasis.blocks {
        emit_block(&mut out, b, 2);
    }

    // AC/Battery
    if !old.stasis.on_ac.is_empty() {
        out.push_str("\n  ac:\n");
        for b in &old.stasis.on_ac {
            emit_block(&mut out, b, 4);
        }
        out.push_str("  end\n");
    }

    if !old.stasis.on_battery.is_empty() {
        out.push_str("\n  battery:\n");
        for b in &old.stasis.on_battery {
            emit_block(&mut out, b, 4);
        }
        out.push_str("  end\n");
    }

    out.push_str("end\n\n");

    // PROFILES -> top-level blocks
    for p in &old.profiles {
        out.push_str(&format!("{}:\n", p.name));
        out.push_str("  mode \"overlay\"\n");

        emit_globals(&mut out, &p.lines, 2);

        for b in &p.blocks {
            emit_block(&mut out, b, 2);
        }

        out.push_str("end\n\n");
    }

    out
}

fn emit_globals(out: &mut String, lines: &[Line], indent: usize) {
    for l in lines {
        // drop removed key
        if l.key == "respect_idle_inhibitors" {
            continue;
        }

        // drop per-block loginctl key (now global)
        if l.key == "use_loginctl" {
            continue;
        }

        // rename notify-before-command -> notify_before_action
        let key = if l.key == "notify_before_command" {
            "notify_before_action".to_string()
        } else if l.key == "listen_browser_dbus_inhibit" {
            "enable_dbus_inhibit".to_string()
        } else if l.key == "debounce_seconds" || l.key == "debounce-seconds" {
            "debounce_seconds".to_string()
        } else {
            l.key.clone()
        };

        // rename notify-seconds-before -> notify_seconds_before
        let key = if key == "notify_seconds_before" || key == "notify-seconds-before" {
            "notify_seconds_before".to_string()
        } else {
            key
        };

        // old used `notify-before-command` and `debounce-seconds` spelling; normalize
        let key = key.replace('-', "_");

        // old had `resume-command` etc inside blocks; globals rarely have those.
        out.push_str(&format!(
            "{:indent$}{} {}\n",
            "",
            key,
            l.raw_value,
            indent = indent
        ));
    }
}

fn emit_block(out: &mut String, b: &OldBlock, indent: usize) {
    let name = b.name.trim().replace('-', "_"); // custom-brightness-instant -> custom_brightness_instant

    out.push_str(&format!("{:indent$}{}:\n", "", name, indent = indent));

    // Collect fields
    let mut timeout: Option<String> = None;
    let mut command: Option<String> = None;
    let mut resume_command: Option<String> = None;
    let mut lock_command: Option<String> = None;
    let mut notification: Option<String> = None;
    let mut notify_seconds_before: Option<String> = None;

    for l in &b.lines {
        match l.key.as_str() {
            "timeout" => timeout = Some(l.raw_value.clone()),
            "command" => command = Some(l.raw_value.clone()),
            "resume_command" | "resume-command" => resume_command = Some(l.raw_value.clone()),
            "lock_command" | "lock-command" => lock_command = Some(l.raw_value.clone()),
            "notification" => notification = Some(l.raw_value.clone()),
            "notify_seconds_before" | "notify-seconds-before" => {
                notify_seconds_before = Some(l.raw_value.clone())
            }
            "use_loginctl" => {
                // dropped; handled globally
            }
            _ => {
                // pass through unknown keys as-is (normalized)
                let k = l.key.replace('-', "_");
                out.push_str(&format!(
                    "{:indent2$}{} {}\n",
                    "",
                    k,
                    l.raw_value,
                    indent2 = indent + 2
                ));
            }
        }
    }

    if let Some(t) = timeout {
        out.push_str(&format!(
            "{:indent2$}timeout {}\n",
            "",
            t,
            indent2 = indent + 2
        ));
    }

    // Lock special case:
    // - Old configs sometimes used command="loginctl lock-session" plus lock_command="<locker>"
    // - New configs: loginctl integration is global (enable_loginctl_integration); lock_screen.command should be the locker.
    let is_lock = name == "lock_screen";
    if is_lock {
        let cmd_is_loginctl = command
            .as_deref()
            .map(|c| c.contains("loginctl lock-session"))
            .unwrap_or(false);

        if cmd_is_loginctl {
            if let Some(lc) = lock_command {
                out.push_str(&format!(
                    "{:indent2$}command {}\n",
                    "",
                    lc,
                    indent2 = indent + 2
                ));
            } else {
                out.push_str(&format!(
                    "{:indent2$}# TODO: lock_screen.command missing (was loginctl lock-session)\n",
                    "",
                    indent2 = indent + 2
                ));
            }
        } else if let Some(c) = command {
            out.push_str(&format!(
                "{:indent2$}command {}\n",
                "",
                c,
                indent2 = indent + 2
            ));
        }
    } else if let Some(c) = command {
        out.push_str(&format!(
            "{:indent2$}command {}\n",
            "",
            c,
            indent2 = indent + 2
        ));
    }

    if let Some(rc) = resume_command {
        out.push_str(&format!(
            "{:indent2$}resume_command {}\n",
            "",
            rc,
            indent2 = indent + 2
        ));
    }

    if let Some(n) = notification {
        out.push_str(&format!(
            "{:indent2$}notification {}\n",
            "",
            n,
            indent2 = indent + 2
        ));
    }
    if let Some(ns) = notify_seconds_before {
        out.push_str(&format!(
            "{:indent2$}notify_seconds_before {}\n",
            "",
            ns,
            indent2 = indent + 2
        ));
    }

    out.push_str(&format!("{:indent$}end\n", "", indent = indent));
}
