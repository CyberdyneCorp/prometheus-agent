use std::time::Duration;

use tracing::Instrument as _;
use tracing_subscriber::prelude::*;

use super::*;

#[test]
fn a_dump_shows_open_spans_and_the_thread_inside_one() {
    let subscriber = tracing_subscriber::registry().with(TaskLayer);
    tracing::subscriber::with_default(subscriber, || {
        let session = tracing::info_span!("session", session_id = "ses_1");
        let _in_session = session.enter();
        let turn = tracing::info_span!("turn", turn_id = "turn_1");
        let _in_turn = turn.enter();
        let text = render("test", None);
        assert!(text.contains("::session session_id=\"ses_1\""), "{text}");
        assert!(text.contains("turn turn_id=\"turn_1\" ("), "{text}");
        assert!(
            text.contains("inside ") && text.contains(" > ") && text.contains("[entered on thread"),
            "{text}"
        );
    });
}

/// Every task with its await point, every thread (also one that never
/// entered a span) and a running blocking job with its thread.
#[test]
fn a_dump_lists_every_thread_and_every_task_with_its_await_point() {
    let (stop_idle, idle_stopped) = std::sync::mpsc::channel::<()>();
    let idle = std::thread::Builder::new()
        .name("pmt-idle-thread".to_owned())
        .spawn(move || {
            let _ = idle_stopped.recv();
        })
        .unwrap();
    let subscriber = tracing_subscriber::registry().with(TaskLayer);
    let text = tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (reply, wait) = tokio::sync::oneshot::channel::<()>();
            let waiter = pmt_core::spawn::spawn(
                async move {
                    let _ = wait
                        .instrument(tracing::info_span!("waiting_for_reply"))
                        .await;
                }
                .instrument(tracing::info_span!("session", session_id = "ses_9")),
            );
            let (started, job_started) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let job = pmt_core::spawn::blocking(move || {
                started.send(()).unwrap();
                let _ = released.recv();
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            job_started.recv().unwrap();
            let text = render("test", None);
            release.send(()).unwrap();
            reply.send(()).unwrap();
            waiter.await.unwrap();
            job.await.unwrap();
            text
        })
    });
    let _ = stop_idle.send(());
    idle.join().unwrap();
    let file = file!();
    assert!(
        text.lines().any(|l| l.contains("async task")
            && l.contains(file)
            && l.contains("awaiting in ")
            && l.contains("::session session_id=\"ses_9\" > ")
            && l.ends_with("::waiting_for_reply")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("blocking job") && l.contains("RUNNING on thread")),
        "{text}"
    );
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    ))]
    {
        assert!(text.contains("\"pmt-idle-thread\""), "{text}");
        assert!(text.contains("      running blocking job #"), "{text}");
    }
}
