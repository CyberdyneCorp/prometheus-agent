//! A temporary home with boards, and helpers shared by the kanban tests.

#![allow(dead_code, unreachable_pub)] // each test binary uses its own subset

use std::cell::RefCell;
use std::process::{Child, Command};

use pmt_kanban::dispatch::{Dispatcher, Settings, SpawnRequest, Spawner};
use pmt_kanban::lease::Holder;
use pmt_kanban::{Actor, Board, Boards, Breaker, NewTask};

/// A fake clock start: 2026-11-01T00:00:00Z.
pub const T0: i64 = 1_793_491_200_000;

pub struct Home {
    pub root: tempfile::TempDir,
    pub boards: Boards,
}

impl Home {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let boards = Boards::open(root.path()).unwrap();
        Self { root, boards }
    }

    pub fn board(&self) -> Board {
        self.boards.board("default").unwrap()
    }
}

pub fn user() -> Actor {
    Actor::user("cli")
}

/// Profiles `writer`, `qa`, `coder` and `ops` exist.
pub fn profiles(name: &str) -> bool {
    matches!(name, "default" | "writer" | "qa" | "coder" | "ops")
}

pub fn create(board: &mut Board, title: &str, parents: &[&str]) -> String {
    let mut new = NewTask::titled(title);
    new.parents = parents.iter().map(|p| (*p).to_owned()).collect();
    new.assignee = Some("writer".to_owned());
    board.create(&user(), &new, &profiles, T0).unwrap().id
}

pub fn settings() -> Settings {
    Settings {
        max_running: 8,
        max_running_per_profile: 2,
        dispatch_profiles: None,
        default_assignee: None,
        breaker: Breaker {
            failure_limit: 2,
            cooldown_ms: 300_000,
        },
        claim_ttl_ms: 900_000,
        review_dispatch: true,
    }
}

/// A spawner that starts `sleep` processes (killed on drop) and records
/// what it was asked.
#[derive(Default)]
pub struct Sleepers {
    pub children: RefCell<Vec<Child>>,
    pub spawned: RefCell<Vec<(String, String)>>,
    pub fail: Option<String>,
}

impl Spawner for Sleepers {
    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<(u32, u64), String> {
        if let Some(message) = &self.fail {
            return Err(message.clone());
        }
        let child = sleeper();
        let pid = child.id();
        self.children.borrow_mut().push(child);
        self.spawned
            .borrow_mut()
            .push((request.task.id.clone(), request.profile.to_owned()));
        Ok((pid, start_of(pid)))
    }
}

impl Drop for Sleepers {
    fn drop(&mut self) {
        for child in self.children.borrow_mut().iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn sleeper() -> Child {
    #[cfg(unix)]
    let child = Command::new("sleep").arg("30").spawn().unwrap();
    #[cfg(windows)]
    let child = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
        .spawn()
        .unwrap();
    child
}

pub fn start_of(pid: u32) -> u64 {
    // The OS may need a moment to list a fresh process.
    for _ in 0..50 {
        if let Some(start) = pmt_core::runtime::process_start_time(pid) {
            return start;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("process {pid} not listed");
}

pub fn dispatcher<'a>(
    home: &'a Home,
    spawner: &'a dyn Spawner,
    settings: Settings,
) -> Dispatcher<'a> {
    Dispatcher {
        boards: &home.boards,
        holder: Holder::current("test"),
        settings,
        profiles: &profiles,
        spawner,
    }
}

pub fn kinds(board: &Board, task: &str) -> Vec<String> {
    board
        .events(Some(task))
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}
