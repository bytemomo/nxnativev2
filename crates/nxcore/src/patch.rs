//! Applying git-format unified diffs (the nxnative patches) without git or patch, on any platform.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Applies a git-format unified diff (text files: modified, new, deleted; `new file mode 100755` sets the executable bit)
pub fn apply(root: &Path, patch: &str) -> Result<()> {
    for file in patch.split("\ndiff --git ").map(|s| s.strip_prefix("diff --git ").unwrap_or(s)) {
        if file.trim().is_empty() {
            continue;
        }
        let lines: Vec<&str> = file.split('\n').collect();
        let mut target = None;
        let mut deleted = false;
        let mut exec = false;
        let mut i = 0;
        while i < lines.len() && !lines[i].starts_with("@@") {
            let l = lines[i];
            if let Some(p) = l.strip_prefix("+++ ") {
                target = (p != "/dev/null").then(|| p.trim_start_matches("b/").to_string());
            } else if l.starts_with("deleted file mode") {
                deleted = true;
            } else if let Some(m) = l.strip_prefix("new file mode ").or_else(|| l.strip_prefix("new mode ")) {
                exec = m.trim().ends_with("755");
            } else if l.starts_with("GIT binary patch") {
                bail!("binary patches are not supported");
            }
            i += 1;
        }
        let header = lines[0];
        let path = match (&target, deleted) {
            (Some(t), _) => t.clone(),
            (None, true) => header.split(' ').next().unwrap_or_default().trim_start_matches("a/").to_string(),
            (None, false) => continue, // a mode change only
        };
        let full = root.join(&path);
        if deleted {
            fs::remove_file(&full).with_context(|| format!("delete {path}"))?;
            continue;
        }
        let old = if full.exists() { fs::read_to_string(&full).with_context(|| format!("read {path}"))? } else { String::new() };
        let new = apply_hunks(&old, &lines[i..]).with_context(|| format!("in {path}"))?;
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, new)?;
        #[cfg(unix)]
        if exec {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&full, fs::Permissions::from_mode(0o755))?;
        }
        let _ = exec;
    }
    Ok(())
}

fn apply_hunks(old: &str, hunks: &[&str]) -> Result<String> {
    let src: Vec<&str> = if old.is_empty() { vec![] } else { old.split_inclusive('\n').collect() };
    let mut out: Vec<String> = vec![];
    let mut pos = 0usize; // next unconsumed line of src
    let mut i = 0;
    while i < hunks.len() {
        let h = hunks[i];
        if !h.starts_with("@@") {
            i += 1;
            continue;
        }
        // @@ -start,len +start,len @@
        let old_start: usize = h.trim_start_matches("@@ -").split([',', ' ']).next().unwrap_or("0").parse().context("bad hunk header")?;
        i += 1;
        let mut before: Vec<String> = vec![];
        let mut after: Vec<String> = vec![];
        let mut last_kind = ' ';
        while i < hunks.len() && !hunks[i].starts_with("@@") {
            let l = hunks[i];
            if let Some(rest) = l.strip_prefix('\\') {
                // "\ No newline at end of file": the previous line has no newline
                let _ = rest;
                let strip = |v: &mut Vec<String>| {
                    if let Some(s) = v.last_mut() {
                        if s.ends_with('\n') {
                            s.pop();
                        }
                    }
                };
                match last_kind {
                    '-' => strip(&mut before),
                    '+' => strip(&mut after),
                    _ => {
                        strip(&mut before);
                        strip(&mut after);
                    }
                }
            } else if let Some(c) = l.chars().next() {
                let text = format!("{}\n", &l[1..]);
                match c {
                    ' ' => {
                        before.push(text.clone());
                        after.push(text);
                    }
                    '-' => before.push(text),
                    '+' => after.push(text),
                    _ => {}
                }
                last_kind = c;
            } else if i + 1 < hunks.len() && !hunks[i + 1].is_empty() {
                // an empty context line whose leading space an editor removed
                before.push("\n".into());
                after.push("\n".into());
            }
            i += 1;
        }
        // find the hunk's old lines at (or near) the stated position
        let want = old_start.saturating_sub(1);
        let matches_at = |at: usize| at + before.len() <= src.len() && before.iter().zip(&src[at..]).all(|(a, b)| a == b);
        let found = (0..=src.len())
            .flat_map(|d| [want.checked_add(d), want.checked_sub(d)])
            .flatten()
            .filter(|&at| at >= pos)
            .find(|&at| matches_at(at));
        let at = match found {
            Some(a) => a,
            None if before.is_empty() => pos.max(want.min(src.len())),
            None => bail!("hunk at line {old_start} does not match"),
        };
        out.extend(src[pos..at].iter().map(|s| s.to_string()));
        out.extend(after);
        pos = at + before.len();
    }
    out.extend(src[pos..].iter().map(|s| s.to_string()));
    Ok(out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_apply() {
        let old = "a\nb\nc\nd\n";
        let patch = ["@@ -1,4 +1,5 @@", " a", "-b", "+B", "+B2", " c", " d", ""];
        assert_eq!(apply_hunks(old, &patch).unwrap(), "a\nB\nB2\nc\nd\n");
    }

    #[test]
    fn new_file_without_newline() {
        let patch = ["@@ -0,0 +1,2 @@", "+x", "+y", "\\ No newline at end of file", ""];
        assert_eq!(apply_hunks("", &patch).unwrap(), "x\ny");
    }
}
