//! Printing through CUPS (`lpstat` / `lp`), used on Linux and macOS. CUPS renders the
//! PDF itself, so the job is the document with all edits, exported by the engine.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::engine::PrintJob;

pub struct Printers {
    pub names: Vec<String>,
    pub default: Option<String>,
}

/// Lists CUPS destinations and the default one.
pub fn list_printers() -> Result<Printers, String> {
    let out = Command::new("lpstat").arg("-e").output().map_err(|e| format!("CUPS is not available (lpstat: {e})"))?;
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let default =
        Command::new("lpstat").arg("-d").output().ok().and_then(|o| parse_default(&String::from_utf8_lossy(&o.stdout)));
    Ok(Printers { names, default })
}

/// Parses `lpstat -d` output ("system default destination: NAME").
fn parse_default(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.split_once("destination:"))
        .map(|(_, name)| name.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// Validates a page range like "1-3, 5, 8-" against `count` pages and normalises it
/// for CUPS ("1-3,5,8-12").
pub fn normalize_ranges(input: &str, count: usize) -> Result<String, String> {
    let mut parts = Vec::new();
    for raw in input.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let parse = |s: &str| -> Result<usize, String> {
            let n: usize = s.trim().parse().map_err(|_| format!("“{part}” is not a page number"))?;
            if n == 0 || n > count {
                return Err(format!("page {n} does not exist (1–{count})"));
            }
            Ok(n)
        };
        match part.split_once('-') {
            Some((a, b)) => {
                let a = if a.trim().is_empty() { 1 } else { parse(a)? };
                let b = if b.trim().is_empty() { count } else { parse(b)? };
                if a > b {
                    return Err(format!("“{part}” runs backwards"));
                }
                parts.push(format!("{a}-{b}"));
            }
            None => parts.push(parse(part)?.to_string()),
        }
    }
    if parts.is_empty() {
        return Err("enter pages like 1-3, 5".into());
    }
    Ok(parts.join(","))
}

/// Builds the `lp` arguments for a job (without the file, which is passed on stdin).
pub fn lp_args(job: &PrintJob) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(p) = &job.printer {
        args.push("-d".into());
        args.push(p.clone());
    }
    args.push("-n".into());
    args.push(job.copies.max(1).to_string());
    if let Some(pages) = &job.pages {
        args.push("-P".into());
        args.push(pages.clone());
    }
    args.push("-o".into());
    args.push(if job.two_sided { "sides=two-sided-long-edge" } else { "sides=one-sided" }.into());
    if job.fit_to_page {
        args.push("-o".into());
        args.push("fit-to-page".into());
    }
    args.push("-t".into());
    args.push(job.title.clone());
    args
}

/// Sends the PDF to CUPS. Returns lp's confirmation (e.g. "request id is X-12").
pub fn submit(job: &PrintJob, pdf: &[u8]) -> Result<String, String> {
    let mut child = Command::new("lp")
        .args(lp_args(job))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run lp: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("lp has no input")?
        .write_all(pdf)
        .map_err(|e| format!("sending the document to lp failed: {e}"))?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(normalize_ranges("1-3, 5", 10).unwrap(), "1-3,5");
        assert_eq!(normalize_ranges("8-", 10).unwrap(), "8-10");
        assert_eq!(normalize_ranges("-2", 10).unwrap(), "1-2");
        assert!(normalize_ranges("0", 10).is_err());
        assert!(normalize_ranges("11", 10).is_err());
        assert!(normalize_ranges("5-3", 10).is_err());
        assert!(normalize_ranges("a", 10).is_err());
        assert!(normalize_ranges(" , ", 10).is_err());
    }

    #[test]
    fn default_printer() {
        assert_eq!(parse_default("system default destination: Office_Laser\n").as_deref(), Some("Office_Laser"));
        assert_eq!(parse_default("no system default destination\n"), None);
    }

    #[test]
    fn args() {
        let job = PrintJob {
            printer: Some("P1".into()),
            copies: 2,
            pages: Some("1-3".into()),
            two_sided: true,
            fit_to_page: true,
            title: "doc.pdf".into(),
        };
        assert_eq!(
            lp_args(&job),
            [
                "-d",
                "P1",
                "-n",
                "2",
                "-P",
                "1-3",
                "-o",
                "sides=two-sided-long-edge",
                "-o",
                "fit-to-page",
                "-t",
                "doc.pdf"
            ]
        );
    }
}
