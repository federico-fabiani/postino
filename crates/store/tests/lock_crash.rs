//! A killed writer must not leave the store locked. The child is this same test binary running one
//! exact test; the parent kills it and then acquires the same lock. This is the crash path that the
//! in-process drop test cannot prove.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use postino_store::{StoreError, WriterLock};

const CHILD_ENV: &str = "POSTINO_LOCK_CRASH_CHILD";

#[test]
fn child_acquires_and_waits() {
    let Ok(path) = std::env::var(CHILD_ENV) else {
        return;
    };
    let _lock = WriterLock::acquire_as(std::path::Path::new(&path), "crash-test", None).expect("child lock");
    println!("locked");
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_killed_writer_releases_the_os_lock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("mail.db");
    let child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "child_acquires_and_waits", "--nocapture"])
        .env(CHILD_ENV, &store)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the child writer");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_busy = false;
    while Instant::now() < deadline {
        if postino_store::read_writer(&store).is_some_and(|owner| owner.label == "crash-test") {
            match WriterLock::acquire(&store) {
                Err(StoreError::StoreBusy { .. }) => observed_busy = true,
                Ok(lock) => {
                    drop(lock);
                }
                Err(error) => panic!("unexpected lock error: {error}"),
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(observed_busy, "the child never acquired the writer lock");
    let mut child = child;
    child.kill().expect("kill the child writer");
    let status = child.wait().expect("wait for the killed child");
    assert!(!status.success());
    let lock = WriterLock::acquire_as(&store, "after-crash", None).expect("the killed writer released the OS lock");
    assert_eq!(lock.owner().label, "after-crash");
}
