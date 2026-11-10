//! Linux and Android threads from `/proc/self/task/<tid>/`: `comm` (the
//! name), `stat` (the scheduler state) and `wchan` (the kernel function a
//! sleeping thread waits in, when the kernel exposes it).

use std::fs;

use super::OsThread;

pub(super) fn current_id() -> u64 {
    // `/proc/thread-self` links to `<pid>/task/<tid>`.
    fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|link| link.file_name()?.to_str()?.parse().ok())
        .unwrap_or_default()
}

pub(super) fn list() -> Option<Vec<OsThread>> {
    let tasks = fs::read_dir("/proc/self/task").ok()?;
    let threads = tasks
        .flatten()
        .filter_map(|entry| {
            let id: u64 = entry.file_name().to_str()?.parse().ok()?;
            let dir = entry.path();
            let read = |file: &str| fs::read_to_string(dir.join(file)).ok();
            let name = read("comm").map(|n| n.trim_end().to_owned());
            let state = read("stat").and_then(|s| state(&s, read("wchan").as_deref()));
            Some(OsThread { id, name, state })
        })
        .collect();
    Some(threads)
}

/// The state letter of a `stat` line (the field after the parenthesised
/// name, which may itself hold spaces and parentheses), spelled out, with
/// the wait channel of a waiting thread.
fn state(stat: &str, wchan: Option<&str>) -> Option<String> {
    let letter = stat.rsplit_once(')')?.1.trim_start().chars().next()?;
    let word = match letter {
        'R' => "running",
        'S' => "sleeping",
        'D' => "uninterruptible wait",
        'T' | 't' => "stopped",
        'Z' => "zombie",
        'I' => "idle",
        _ => return Some(letter.to_string()),
    };
    let wchan = wchan.map(str::trim).filter(|w| !w.is_empty() && *w != "0");
    Some(match wchan {
        Some(w) if letter != 'R' => format!("{word} in {w}"),
        _ => word.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::state;

    #[test]
    fn a_stat_line_names_the_state_and_the_wait_channel() {
        let stat = "4242 (tokio (rt) w) S 1 4242 0 0 -1";
        assert_eq!(
            state(stat, Some("futex_wait_queue\n")).as_deref(),
            Some("sleeping in futex_wait_queue")
        );
        assert_eq!(state(stat, Some("0")).as_deref(), Some("sleeping"));
        assert_eq!(
            state("1 (x) R 0", Some("do_epoll_wait")).as_deref(),
            Some("running")
        );
    }
}
