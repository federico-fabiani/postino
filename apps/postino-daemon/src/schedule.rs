//! The background scheduler: outbox due/lease/append, sync interval and manual runs, P9c rules
//! after sync, one P9a index batch per tick and a bounded blob GC. It sleeps until the next due
//! instant; there is no polling loop.

use std::sync::Arc;
use std::time::Duration;

use postino_core::{OpError, OutboxRunReport};
use serde_json::json;
use tokio::time::{Instant, sleep_until};

use crate::DaemonState;
use crate::dispatch::error_category;

/// How soon to retry an outbox pass that left due work (a retry/backoff or an append).
const OUTBOX_DRAIN: Duration = Duration::from_millis(100);
/// A failed background step is retried no sooner than this; it never becomes a busy loop.
const RETRY: Duration = Duration::from_secs(30);

pub async fn run(state: Arc<DaemonState>) {
    recover(&state).await;
    let mut next_sync =
        if state.config.sync_on_start { Instant::now() + Duration::from_secs(2) } else { Instant::now() + state.config.sync_interval };
    let mut next_gc = Instant::now() + state.config.gc_interval;
    // Give the first client a moment to subscribe to index progress; the daemon is already serving.
    let mut next_index = Instant::now() + Duration::from_millis(250);
    let mut outbox_wake = if state.config.outbox_on_start { Instant::now() } else { Instant::now() + Duration::from_secs(24 * 60 * 60) };
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        let now_for_index = Instant::now();
        if state.index_dirty() && next_index > now_for_index + state.config.index_tick {
            // A mutation marked the index dirty after a completed build; do not wait for the 24 h
            // idle recheck.
            next_index = now_for_index;
        }
        let mut deadline = next_sync.min(next_gc).min(outbox_wake);
        if state.index_dirty() || next_index <= deadline {
            deadline = deadline.min(next_index);
        }
        let wake = deadline.max(Instant::now());
        let mut outbox_notified = false;
        let mut sync_requested = false;
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.sync_notify.notified() => {
                sync_requested = true;
                state.count_manual_sync();
            }
            _ = state.outbox_notify.notified() => {
                outbox_notified = true;
            }
            _ = sleep_until(wake) => {}
        }
        state.count_wake();
        let now = Instant::now();
        if outbox_notified || now >= outbox_wake {
            outbox_wake = run_outbox(&state).await;
        }
        if sync_requested || now >= next_sync {
            run_sync(&state).await;
            next_sync = Instant::now() + state.config.sync_interval + jitter(&state);
        }
        if state.index_dirty() && now >= next_index {
            match run_index(&state).await {
                true => {
                    let failed = state.status.lock().map(|status| status.index_error).unwrap_or(false);
                    next_index = Instant::now() + if failed { RETRY } else { state.config.index_tick };
                }
                false => next_index = Instant::now() + Duration::from_secs(24 * 60 * 60),
            }
        }
        let gc_due = state.status.lock().map(|status| status.gc_due).unwrap_or(false);
        if now >= next_gc || gc_due {
            run_gc(&state).await;
            next_gc = Instant::now() + state.config.gc_interval;
        }
    }
}

/// Read the persisted outbox once at startup. A crashed `sending` lease is not reset here; the
/// normal outbox pass does it, and every queued/scheduled/append_pending row is left in the store.
async fn recover(state: &DaemonState) {
    let outcome = state
        .with_engine(|engine| {
            let rows = engine.outbox_list(None, 500, 0);
            let next = engine.next_outbox_wake().ok().flatten();
            (rows, next)
        })
        .await;
    let (rows, next) = match outcome {
        Ok(value) => value,
        Err(_) => {
            log::warn!("daemon recovery could not read the outbox");
            return;
        }
    };
    if let Ok(mut status) = state.status.lock() {
        status.outbox_next_wake = next;
    }
    let rows = match rows {
        Ok(rows) => rows,
        Err(_) => {
            log::warn!("daemon recovery could not read the outbox");
            return;
        }
    };
    let interrupted = rows
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.state,
                postino_store::OutboxState::Sending | postino_store::OutboxState::AppendPending | postino_store::OutboxState::Uncertain
            )
        })
        .count();
    if interrupted > 0 || !rows.entries.is_empty() {
        let sentence = "daemon start: every outbox row is kept; an expired submission lease becomes uncertain and is never resent automatically";
        log::info!("{sentence}");
        state.note_recovery(sentence);
    }
    // An outbox failure that happened before the restart still owns the tray icon and one notice.
    crate::tray_host::after_outbox(state).await;
}

async fn run_outbox(state: &DaemonState) -> Instant {
    let now_ms = crate::now_ms();
    let outcome = state
        .with_engine(move |engine| {
            let report = engine.process_outbox(now_ms);
            let next = engine.next_outbox_wake().ok().flatten();
            (report, next)
        })
        .await;
    let (report, next) = match outcome {
        Ok(value) => value,
        Err(error) => {
            state.note_error("the outbox pass could not run".to_owned());
            log::warn!("daemon outbox pass failed: {error}");
            return Instant::now() + RETRY;
        }
    };
    if let Ok(mut status) = state.status.lock() {
        status.outbox_next_wake = next;
    }
    match report {
        Ok(report) => {
            publish_outbox(state, &report);
            crate::tray_host::after_outbox(state).await;
            if report.has_due_work() { Instant::now() + OUTBOX_DRAIN } else { instant_for(next) }
        }
        Err(error) => {
            let sentence = error_category(&error);
            state.note_error(format!("the outbox pass could not run: {sentence}"));
            log::warn!("daemon outbox pass failed: {sentence}");
            Instant::now() + RETRY
        }
    }
}

fn publish_outbox(state: &DaemonState, report: &OutboxRunReport) {
    state.publish(
        "outbox",
        json!({
            "processed": report.processed,
            "sent": report.sent,
            "append_pending": report.append_pending,
            "retried": report.retried,
            "failed": report.failed,
            "uncertain": report.uncertain,
            "halted": report.halted.len(),
        }),
    );
}

async fn run_sync(state: &DaemonState) {
    if state.is_sync_active() {
        return;
    }
    let accounts = match state.account_ids().await {
        Ok(accounts) => accounts,
        Err(error) => {
            state.note_error("the account list could not be read".to_owned());
            log::warn!("daemon sync could not list accounts: {error}");
            return;
        }
    };
    for account in accounts {
        if state.shutdown.is_cancelled() {
            return;
        }
        let request = format!("scheduler-sync:{account}");
        let flag = state.begin_request(&request);
        let _guard = SchedulerSyncGuard { state: state.clone(), request: request.clone(), account: account.clone() };
        state.set_sync_active(true, Some(&account));
        state.publish("sync", json!({ "phase": "started", "account": account }));
        let watcher = state.spawn_sync_watcher(account.clone());
        let chunk = state.config.sync_chunk_messages.max(1).min(u32::MAX as usize) as u32;
        let mut aggregate = postino_core::SyncReport {
            account: account.clone(),
            folders: 0,
            fetched: 0,
            updated_flags: 0,
            removed: 0,
            attachments_stored: 0,
            warnings: Vec::new(),
        };
        let outcome = loop {
            let chunk_flag = Arc::clone(&flag);
            let chunk_outcome = state
                .with_engine({
                    let account = account.clone();
                    move |engine| {
                        engine.sync_now_with_cancel_limits(
                            Some(&account),
                            &chunk_flag,
                            postino_sync::SyncLimits { max_messages: usize::try_from(chunk).unwrap_or(usize::MAX), ..Default::default() },
                        )
                    }
                })
                .await;
            let report = match chunk_outcome {
                Ok(Ok(report)) => report,
                Ok(Err(error)) => break Err(error),
                Err(error) => break Err(postino_core::OpError::Failed(error.to_string())),
            };
            aggregate.folders = aggregate.folders.max(report.folders);
            aggregate.fetched = aggregate.fetched.saturating_add(report.fetched);
            aggregate.updated_flags = aggregate.updated_flags.saturating_add(report.updated_flags);
            aggregate.removed = aggregate.removed.saturating_add(report.removed);
            aggregate.attachments_stored = aggregate.attachments_stored.saturating_add(report.attachments_stored);
            aggregate.warnings.extend(report.warnings);
            if report.fetched < chunk {
                break Ok(aggregate);
            }
            // The engine lock is released now. Drain a due outbox row and one index batch before the
            // next chunk, so a scheduled send is not stuck behind a long initial sync.
            let _ = run_outbox(state).await;
            if state.index_dirty() {
                let _ = run_index(state).await;
            }
            if state.shutdown.is_cancelled() {
                break Err(postino_core::OpError::Failed("the daemon is shutting down".to_owned()));
            }
        };
        drop(watcher);
        match outcome {
            Ok(report) => {
                state.publish(
                    "sync",
                    json!({
                        "phase": "finished",
                        "account": report.account,
                        "folders": report.folders,
                        "fetched": report.fetched,
                        "updated_flags": report.updated_flags,
                        "removed": report.removed,
                        "attachments_stored": report.attachments_stored,
                    }),
                );
                state.mark_index_dirty();
                if state.rules_enabled(&report.account).await {
                    // The post-sync rules pass changed labels inside the engine: the app must see
                    // that without waiting for the next blob GC.
                    state.publish("rules", json!({ "account": report.account, "source": "sync" }));
                }
                if let Ok(mut status) = state.status.lock() {
                    status.last_sync_at = Some(crate::now_ms());
                    status.gc_due = true;
                }
                state.set_network_error(false);
                crate::tray_host::after_sync(state).await;
            }
            Err(error) => {
                let sentence = error_category(&error);
                state.note_error(format!("sync failed: {sentence}"));
                state.set_network_error(true);
                log::warn!("daemon scheduled sync failed: {sentence}");
                state.publish("sync", json!({ "phase": "failed", "account": account, "error": sentence }));
            }
        }
    }
    state.wake_scheduler();
}

/// Returns true when another batch is still needed.
async fn run_index(state: &DaemonState) -> bool {
    let outcome = state.with_engine(|engine| engine.advance_search_index()).await;
    match outcome {
        Ok(Ok(status)) => {
            if let Some(status) = &status {
                state.publish(
                    "index",
                    json!({
                        "state": status.state,
                        "indexed": status.indexed,
                        "total": status.total,
                        "ready": status.ready,
                        "last_error": status.last_error,
                        "size_bytes": status.size_bytes,
                        "updated_at": status.updated_at,
                    }),
                );
                if let Ok(mut live) = state.status.lock() {
                    live.last_index = Some(status.clone());
                    live.index_error = false;
                }
                if !status.ready {
                    return true;
                }
            }
            state.take_index_dirty();
            false
        }
        Ok(Err(error)) => {
            let sentence = error_category(&error);
            state.note_error(format!("the search index could not advance: {sentence}"));
            if let Ok(mut live) = state.status.lock() {
                live.index_error = true;
            }
            log::warn!("daemon index batch failed: {sentence}");
            true
        }
        Err(error) => {
            if let Ok(mut live) = state.status.lock() {
                live.index_error = true;
            }
            log::warn!("daemon index task failed: {error}");
            true
        }
    }
}

async fn run_gc(state: &DaemonState) {
    let limit = state.config.gc_batch;
    match state.with_engine(move |engine| engine.gc_blobs(limit)).await {
        Ok(Ok(report)) => {
            state
                .publish("gc", json!({ "scanned": report.scanned, "removed": report.removed, "referenced": report.referenced, "kept": report.kept }));
            if let Ok(mut status) = state.status.lock() {
                status.gc_due = false;
            }
        }
        Ok(Err(OpError::Failed(_))) => {
            state.note_error("blob GC could not run".to_owned());
            log::warn!("daemon blob GC skipped");
            if let Ok(mut status) = state.status.lock() {
                status.gc_due = false;
            }
        }
        Ok(Err(error)) => {
            let sentence = error_category(&error);
            state.note_error(format!("blob GC could not run: {sentence}"));
            log::warn!("daemon blob GC failed: {sentence}");
            if let Ok(mut status) = state.status.lock() {
                status.gc_due = false;
            }
        }
        Err(error) => {
            log::warn!("daemon GC task failed: {error}");
            if let Ok(mut status) = state.status.lock() {
                status.gc_due = false;
            }
        }
    }
}

struct SchedulerSyncGuard {
    state: DaemonState,
    request: String,
    account: String,
}

impl Drop for SchedulerSyncGuard {
    fn drop(&mut self) {
        self.state.finish_request(&self.request);
        self.state.set_sync_active(false, Some(&self.account));
    }
}

fn instant_for(wake_ms: Option<i64>) -> Instant {
    match wake_ms {
        Some(wake) => {
            let now = crate::now_ms();
            let delay = u64::try_from(wake.saturating_sub(now)).unwrap_or(0);
            Instant::now() + Duration::from_millis(delay.max(1))
        }
        None => Instant::now() + Duration::from_secs(24 * 60 * 60),
    }
}

/// A small deterministic jitter (0-10% of the interval) so several daemons started together do not
/// stampede the same provider. Tests set `sync_jitter = false`.
fn jitter(state: &DaemonState) -> Duration {
    if !state.config.sync_jitter {
        return Duration::ZERO;
    }
    let interval = state.config.sync_interval.as_millis().max(1);
    let seed = crate::now_ms().unsigned_abs().wrapping_add(u64::from(std::process::id()));
    let part = seed % 11;
    let millis = u128::from(part) * interval / 100;
    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}
