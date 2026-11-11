//! Where a QEMU test run's time goes. Each line of the serial console is
//! stamped with the time it arrived (to the 20 ms `run_qemu` polls at),
//! and every `<who>: ` line is charged the time until the next one: a
//! `run` or `sh` line the time of that test, up to its verdict. The
//! report is a line of totals and the slowest steps, and the stamped
//! console goes to `target/qemu-times/<run>.txt`.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// How many of the slowest steps the summary names.
const SLOWEST: usize = 6;

/// Prints the summary for run `name` of `who` (`kernel` or `probe`) and
/// writes the stamped console beside the other runs'.
pub fn report(ws: &Path, who: &str, name: &str, lines: &[(Duration, String)]) {
    let prefix = format!("{who}: ");
    let marks: Vec<(Duration, &str)> = lines
        .iter()
        .filter(|(_, l)| l.starts_with(&prefix))
        .map(|(t, l)| (*t, l.as_str()))
        .collect();
    let end = lines.last().map_or(Duration::ZERO, |l| l.0);
    let mut steps: Vec<(Duration, &str)> = marks
        .iter()
        .enumerate()
        .map(|(i, (t, l))| (marks.get(i + 1).map_or(end, |n| n.0).saturating_sub(*t), *l))
        .collect();
    let first_test = marks
        .iter()
        .find(|(_, l)| l.starts_with(&format!("{prefix}run ")))
        .map_or(end, |m| m.0);
    let sum = |kind: &str| -> (Duration, usize) {
        let k = format!("{prefix}{kind} ");
        let s: Vec<Duration> = steps
            .iter()
            .filter(|(_, l)| l.starts_with(&k))
            .map(|s| s.0)
            .collect();
        (s.iter().sum(), s.len())
    };
    let (runs, nruns) = sum("run");
    let (sh, nsh) = sum("sh");
    steps.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = format!(
        "{who}: {name}: {:.1}s: boot to the first test {:.1}s, {nruns} runs {:.1}s, {nsh} sh scripts {:.1}s; slowest:",
        end.as_secs_f64(),
        first_test.as_secs_f64(),
        runs.as_secs_f64(),
        sh.as_secs_f64(),
    );
    for (d, l) in steps.iter().take(SLOWEST) {
        let _ = write!(
            out,
            " {} {:.1}s;",
            l.trim_start_matches(&prefix),
            d.as_secs_f64()
        );
    }
    out.pop();
    eprintln!("{out}");
    let dir = ws.join("target/qemu-times");
    let file = dir.join(format!("{who}-{}.txt", name.replace(' ', "-")));
    let text: String = lines
        .iter()
        .map(|(t, l)| format!("{:8.2} {l}\n", t.as_secs_f64()))
        .collect();
    if let Err(e) = fs::create_dir_all(&dir).and_then(|()| fs::write(&file, text)) {
        eprintln!("{who}: {}: {e}", file.display());
    }
}
