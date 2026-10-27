//! Local rules (P9c): CRUD, preview, execution, journal and undo.
//!
//! A rule is a validated search query plus 1-5 actions. Execution is per-rule in `priority ASC,
//! id ASC` order: the next rule queries the store again, so it sees what an earlier rule moved,
//! labelled or marked. A `stop` rule records the messages it matched and later rules skip them.
//!
//! Rules never touch Drafts, Sent or Trash, never run while the app is closed (there is no daemon)
//! and never open SMTP themselves: a forward becomes one row in the P7 outbox, with a deterministic
//! `rule:<rule>:<message>` key. The journal stores ids, folders, labels and booleans only.

use std::collections::{BTreeSet, HashMap};

use chrono::Utc;
use postino_store::{
    Rule as StoredRule, RuleActionRecord, RuleCandidateFilter, RuleGuard, RuleMessageState, RuleRun as StoredRuleRun, Store, StoreError,
};
use serde_json::{Value, json};

use crate::engine::{Engine, MAX_ATTACHMENT_TOTAL_BYTES, OpError, validate_addresses};
use crate::model::{
    Account, Address, FolderKind, Message, MessageSummary, Rule, RuleAction, RuleEffectStatus, RulePreviewEffect, RulePreviewRow, RuleRun,
    RuleRunScope,
};
use crate::outbox::OutboxEnvelope;
use crate::store_map;

/// Rules one account may hold.
pub const MAX_RULES: u32 = 100;
/// Actions one rule may hold.
pub const MAX_RULE_ACTIONS: usize = 5;
/// Recipients one forward action may hold.
pub const MAX_FORWARD_RECIPIENTS: usize = 10;
/// Messages one run may consider. `new` marks what it considered; the rest waits for the next run.
pub const MAX_RUN_MESSAGES: u32 = 2_000;
/// Forwards one run may queue or resolve.
pub const MAX_RUN_FORWARDS: u32 = 50;
/// Preview rows returned by the operation. The UI shows the first 20.
pub const MAX_PREVIEW_ROWS: u32 = 50;
/// Journal runs kept per account.
pub const MAX_KEPT_RUNS: u32 = 20;

/// What one run did, plus the fixed errors it could not hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRunOutcome {
    pub run: RuleRun,
    pub errors: Vec<String>,
}

/// What one undo restored and what it deliberately did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleUndoOutcome {
    pub run: RuleRun,
    pub restored: u32,
    pub skipped: u32,
    /// Outbox rows of forwards in the run. A forward is never undone.
    pub forwards: Vec<String>,
}

/// The internal result of one action.
struct Applied {
    changed: bool,
    /// The journal value. `None` means a no-op that needs no row.
    value: Option<String>,
}

/// The result of resolving one forward to its outbox row.
struct ForwardOutcome {
    outbox: String,
    /// False when an earlier run already queued the same `rule:message` row.
    created: bool,
}

impl Engine {
    // -----------------------------------------------------------------------------------------
    // Listing and CRUD
    // -----------------------------------------------------------------------------------------

    /// Rules of one account in execution order, with their targets checked against the current
    /// folder and label registries. A row whose target disappeared stays visible as `valid: false`.
    pub fn rules_list(&self, account: Option<&str>) -> Result<Vec<Rule>, OpError> {
        let account = self.requested_account_id(account, "rules_list")?;
        let mut rows = self.rules.iter().filter(|rule| rule.account_id == account).cloned().collect::<Vec<_>>();
        for rule in &mut rows {
            let error = self.rule_validity(rule);
            rule.valid = error.is_none();
            rule.error = error;
        }
        rows.sort_by(|left, right| left.priority.cmp(&right.priority).then(left.id.cmp(&right.id)));
        Ok(rows)
    }

    /// Re-read one account's rules from the store. The demo has no second writer, so it is a no-op.
    pub fn reload_rules(&mut self, account: Option<&str>) -> Result<(), OpError> {
        let account = self.requested_account_id(account, "rules_list")?;
        let Some(store) = self.store() else { return Ok(()) };
        let rows = store.list_rules(&account).map_err(store_map::read_error)?.into_iter().map(store_map::rule).collect::<Vec<_>>();

        self.rules.retain(|rule| rule.account_id != account);
        self.rules.extend(rows);
        self.rules
            .sort_by(|left, right| left.account_id.cmp(&right.account_id).then(left.priority.cmp(&right.priority)).then(left.id.cmp(&right.id)));
        Ok(())
    }

    /// Create one rule. Names are unique per account ignoring case; actions and every target are
    /// validated before the row exists, and the 100-rule cap is one store transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn create_rule(
        &mut self,
        account: Option<&str>,
        name: &str,
        query: &str,
        actions: Vec<RuleAction>,
        priority: Option<i64>,
        stop: bool,
        enabled: bool,
    ) -> Result<Rule, OpError> {
        let account = self.account_for(account, "rules_create")?;
        let name = validate_rule_name(name)?;
        let query = validate_rule_query(self, &account.id, query)?;
        let actions = validate_rule_actions(self, &account.id, &actions)?;
        if self.rules.iter().any(|rule| rule.account_id == account.id && rule.name.eq_ignore_ascii_case(&name)) {
            return Err(OpError::InvalidArgs(format!("a rule named {name:?} already exists; names are unique ignoring case")));
        }
        let count = self.rules.iter().filter(|rule| rule.account_id == account.id).count();
        if u32::try_from(count).unwrap_or(u32::MAX) >= MAX_RULES {
            return Err(rule_limit_error(&account.id));
        }
        if let Some(store) = self.store()
            && store.count_rules(&account.id).map_err(store_map::read_error)? >= MAX_RULES
        {
            return Err(rule_limit_error(&account.id));
        }
        let id = if let Some(store) = self.store() {
            store.allocate_rule_id().map_err(store_map::write_error)?
        } else {
            let id = format!("rule_{}", self.next_rule_seq);
            self.next_rule_seq = self.next_rule_seq.saturating_add(1);
            id
        };
        let now = Utc::now().timestamp_millis();
        let actions_json = serde_json::to_string(&actions).map_err(|error| OpError::Failed(format!("could not encode the rule actions: {error}")))?;
        let stored = StoredRule {
            id: id.clone(),
            account_id: account.id.clone(),
            name: name.clone(),
            enabled,
            priority: priority.unwrap_or(0),
            stop,
            query: query.clone(),
            actions_json,
            created_at: now,
            updated_at: now,
        };
        if let Some(store) = self.store() {
            store.create_rule_limited(&stored, MAX_RULES).map_err(store_map::write_error)?;
        }
        let mut rule = store_map::rule(stored);
        rule.valid = true;
        rule.error = None;
        self.rules.push(rule.clone());
        self.rules
            .sort_by(|left, right| left.account_id.cmp(&right.account_id).then(left.priority.cmp(&right.priority)).then(left.id.cmp(&right.id)));
        self.bump_revision();
        self.rules_list(Some(&account.id))?
            .into_iter()
            .find(|candidate| candidate.id == id)
            .ok_or_else(|| OpError::Failed("the new rule disappeared from the list".to_owned()))
    }

    /// Change the editable fields. `None` keeps the current value; `actions: Some` replaces the
    /// whole list and is validated like a create.
    #[allow(clippy::too_many_arguments)]
    pub fn update_rule(
        &mut self,
        account: Option<&str>,
        id: &str,
        name: Option<&str>,
        query: Option<&str>,
        actions: Option<Vec<RuleAction>>,
        priority: Option<i64>,
        stop: Option<bool>,
        enabled: Option<bool>,
    ) -> Result<Rule, OpError> {
        if name.is_none() && query.is_none() && actions.is_none() && priority.is_none() && stop.is_none() && enabled.is_none() {
            return Err(OpError::InvalidArgs("provide name, query, actions, priority, stop and/or enabled".to_owned()));
        }
        let account = self.account_for(account, "rules_update")?;
        if !self.rules.iter().any(|rule| rule.id == id && rule.account_id == account.id) {
            self.reload_rules(Some(&account.id))?;
        }
        let index =
            self.rules.iter().position(|rule| rule.id == id).filter(|index| self.rules.get(*index).is_some_and(|rule| rule.account_id == account.id));
        let Some(index) = index else { return Err(unknown_rule(id)) };
        let current = self.rules.get(index).cloned().ok_or_else(|| unknown_rule(id))?;
        let resolved_name = match name {
            Some(name) => validate_rule_name(name)?,
            None => current.name.clone(),
        };
        if self.rules.iter().any(|rule| rule.id != id && rule.account_id == account.id && rule.name.eq_ignore_ascii_case(&resolved_name)) {
            return Err(OpError::InvalidArgs(format!("a rule named {resolved_name:?} already exists; names are unique ignoring case")));
        }
        let resolved_query = match query {
            Some(query) => validate_rule_query(self, &account.id, query)?,
            None => current.query.clone(),
        };
        let resolved_actions = match actions {
            Some(actions) => validate_rule_actions(self, &account.id, &actions)?,
            None => current.actions.clone(),
        };
        let actions_json =
            serde_json::to_string(&resolved_actions).map_err(|error| OpError::Failed(format!("could not encode the rule actions: {error}")))?;
        let stored = StoredRule {
            id: current.id.clone(),
            account_id: current.account_id.clone(),
            name: resolved_name,
            enabled: enabled.unwrap_or(current.enabled),
            priority: priority.unwrap_or(current.priority),
            stop: stop.unwrap_or(current.stop),
            query: resolved_query,
            actions_json,
            created_at: current.created_at,
            updated_at: Utc::now().timestamp_millis(),
        };
        if let Some(store) = self.store() {
            store.update_rule(&stored).map_err(store_map::write_error)?;
        }
        let updated = store_map::rule(stored);
        if let Some(slot) = self.rules.get_mut(index) {
            *slot = updated;
        }
        self.bump_revision();
        self.rules_list(Some(&account.id))?.into_iter().find(|candidate| candidate.id == id).ok_or_else(|| unknown_rule(id))
    }

    /// Delete one rule. The journal of past runs stays: it is history, not configuration.
    pub fn delete_rule(&mut self, account: Option<&str>, id: &str) -> Result<Rule, OpError> {
        let account = self.account_for(account, "rules_delete")?;
        if !self.rules.iter().any(|rule| rule.id == id && rule.account_id == account.id) {
            self.reload_rules(Some(&account.id))?;
        }
        let index = self.rules.iter().position(|rule| rule.id == id && rule.account_id == account.id).ok_or_else(|| unknown_rule(id))?;
        let removed = self.rules.get(index).cloned().ok_or_else(|| unknown_rule(id))?;
        if let Some(store) = self.store() {
            store.delete_rule(id).map_err(store_map::write_error)?;
        }
        self.rules.remove(index);
        self.bump_revision();
        let mut removed = removed;
        let error = self.rule_validity(&removed);
        removed.valid = error.is_none();
        removed.error = error;
        Ok(removed)
    }

    /// Check every target and the query against the current registries. `Some(sentence)` means the
    /// row is stale; `None` means it can run now.
    fn rule_validity(&self, rule: &Rule) -> Option<String> {
        if let Err(error) = crate::ops::validate_search_query(self, &rule.query, Some(&rule.account_id)) {
            return Some(error.to_string());
        }
        validate_rule_actions(self, &rule.account_id, &rule.actions).err().map(|error| error.to_string())
    }

    fn account_for(&self, requested: Option<&str>, operation: &str) -> Result<Account, OpError> {
        let id = self.requested_account_id(requested, operation)?;
        self.accounts().iter().find(|account| account.id == id).cloned().ok_or_else(|| unknown_account(&id))
    }

    // -----------------------------------------------------------------------------------------
    // Preview
    // -----------------------------------------------------------------------------------------

    /// What a hypothetical rule would do to the first `limit` received messages. Nothing is
    /// written: no journal, no flag, no outbox row, no `rules_applied_at`.
    pub fn rules_preview(
        &self,
        account: Option<&str>,
        query: &str,
        actions: &[RuleAction],
        limit: Option<u32>,
    ) -> Result<(u32, bool, Vec<RulePreviewRow>), OpError> {
        let account = self.account_for(account, "rules_preview")?;
        let query = validate_rule_query(self, &account.id, query)?;
        let actions = validate_rule_actions(self, &account.id, actions)?;
        let limit = limit.unwrap_or(MAX_PREVIEW_ROWS).clamp(1, MAX_PREVIEW_ROWS);
        let parsed = crate::search::parse(&query)
            .map_err(|error| OpError::InvalidArgs(format!("search query: {} at position {}", error.code, error.position)))?;
        if let Some(store) = self.store() {
            let expr = crate::ops::translate_search(&parsed.expr, self, Some(&account.id))?;
            let page = store
                .rule_candidates(&RuleCandidateFilter { account_id: account.id.clone(), expr: Some(expr), since: 0, new_only: false, limit })
                .map_err(store_map::read_error)?;
            let mut rows = Vec::with_capacity(page.messages.len());
            for message in &page.messages {
                let summary = crate::store_map::summary(message.clone());
                let guard = if actions.iter().any(|action| matches!(action, RuleAction::Forward { .. })) {
                    store.rule_guard(&summary.id).map_err(store_map::read_error)?
                } else {
                    RuleGuard::default()
                };
                let folder_kind = self.folders().iter().find(|folder| folder.id == summary.folder_id).map(|folder| folder.kind);
                let effects = preview_effects(&actions, &summary, folder_kind, guard);
                rows.push(RulePreviewRow { message: summary, effects });
            }
            let truncated = page.total_capped || usize::try_from(page.total).unwrap_or(usize::MAX) > rows.len();
            return Ok((page.total, truncated, rows));
        }
        let (total, rows) = self.demo_preview(&account.id, &parsed.expr, &actions, limit)?;
        let truncated = total > rows.len();
        Ok((u32::try_from(total).unwrap_or(u32::MAX), truncated, rows))
    }

    fn demo_preview(
        &self,
        account_id: &str,
        expr: &crate::search::Expr,
        actions: &[RuleAction],
        limit: u32,
    ) -> Result<(usize, Vec<RulePreviewRow>), OpError> {
        let folder_ids =
            self.folders().iter().filter(|folder| folder.account_id == account_id).map(|folder| folder.id.as_str()).collect::<BTreeSet<_>>();
        let mut matches = self
            .messages()
            .iter()
            .filter(|message| folder_ids.contains(message.folder_id.as_str()))
            .filter(|message| {
                self.folders()
                    .iter()
                    .find(|folder| folder.id == message.folder_id)
                    .is_some_and(|folder| !matches!(folder.kind, FolderKind::Drafts | FolderKind::Sent | FolderKind::Trash))
            })
            .filter_map(|summary| self.message(&summary.id))
            .filter(|message| crate::ops::demo_matches(expr, message, self))
            .collect::<Vec<_>>();
        matches.sort_by_key(|message| (message.summary.date, message.summary.id.clone()));
        let total = matches.len();
        let rows = matches
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|message| {
                let folder_kind = self.folders().iter().find(|folder| folder.id == message.summary.folder_id).map(|folder| folder.kind);
                let effects = preview_effects(actions, &message.summary, folder_kind, RuleGuard::default());
                RulePreviewRow { message: message.summary, effects }
            })
            .collect();
        Ok((total, rows))
    }

    // -----------------------------------------------------------------------------------------
    // Run
    // -----------------------------------------------------------------------------------------

    /// Run every enabled rule once. `new` considers only messages no earlier run applied and marks
    /// what it saw; `all` re-evaluates the first 2 000 received messages since the earliest rule was
    /// created. A failed action is journaled as an error and aborts only that rule's remaining
    /// actions for that message. Rules never fail `sync_now`.
    pub fn run_rules(&mut self, account: Option<&str>, scope: RuleRunScope) -> Result<RuleRunOutcome, OpError> {
        let account = self.account_for(account, "rules_run")?;
        if self.is_persistent() {
            self.reload_rules(Some(&account.id))?;
        }
        let mut enabled = self.rules.iter().filter(|rule| rule.account_id == account.id && rule.enabled).cloned().collect::<Vec<_>>();
        enabled.sort_by(|left, right| left.priority.cmp(&right.priority).then(left.id.cmp(&right.id)));
        let outcome = match self.store() {
            Some(store) => run_store(self, store, &account, &enabled, scope)?,
            None => self.run_demo(&account, &enabled, scope)?,
        };
        if self.is_persistent() {
            self.reload_mailbox()?;
        }
        Ok(outcome)
    }

    fn run_demo(&mut self, account: &Account, rules: &[Rule], scope: RuleRunScope) -> Result<RuleRunOutcome, OpError> {
        let started = Utc::now().timestamp_millis();
        let run_id = format!("rule_run_{}", self.next_rule_run_seq);
        self.next_rule_run_seq = self.next_rule_run_seq.saturating_add(1);
        let mut run = RuleRun {
            id: run_id.clone(),
            account_id: account.id.clone(),
            started_at: started,
            finished_at: None,
            mode: scope,
            rule_count: u32::try_from(rules.len()).unwrap_or(u32::MAX),
            matched: 0,
            changed: 0,
            errors: 0,
            undone: false,
        };
        if rules.is_empty() {
            run.finished_at = Some(Utc::now().timestamp_millis());
            self.rule_runs.insert(0, run.clone());
            self.rule_runs.truncate(usize::try_from(MAX_KEPT_RUNS).unwrap_or(MAX_KEPT_RUNS as usize));
            self.bump_revision();
            return Ok(RuleRunOutcome { run, errors: Vec::new() });
        }
        let folder_ids =
            self.folders().iter().filter(|folder| folder.account_id == account.id).map(|folder| folder.id.clone()).collect::<BTreeSet<_>>();
        let eligible = |engine: &Engine, folder_ids: &BTreeSet<String>, summary: &MessageSummary| {
            folder_ids.contains(&summary.folder_id)
                && engine
                    .folders()
                    .iter()
                    .find(|folder| folder.id == summary.folder_id)
                    .is_some_and(|folder| !matches!(folder.kind, FolderKind::Drafts | FolderKind::Sent | FolderKind::Trash))
        };
        let universe = self
            .messages()
            .iter()
            .filter(|message| eligible(self, &folder_ids, message))
            .filter(|message| scope == RuleRunScope::All || !self.demo_rules_applied.contains(&message.id))
            .map(|message| message.id.clone())
            .take(usize::try_from(MAX_RUN_MESSAGES).unwrap_or(usize::MAX))
            .collect::<BTreeSet<_>>();
        let mut errors = Vec::new();
        let mut stopped = BTreeSet::new();
        let mut actions_log = Vec::new();
        let mut ordinal = 1u32;
        for rule in rules {
            let parsed = match crate::search::parse(&rule.query) {
                Ok(parsed) => parsed,
                Err(error) => {
                    errors.push(format!("rule {:?}: the stored query is not readable ({} at position {})", rule.id, error.code, error.position));
                    continue;
                }
            };
            let mut candidates = self
                .messages()
                .iter()
                .filter(|message| universe.contains(&message.id))
                .filter(|message| eligible(self, &folder_ids, message))
                .filter_map(|summary| self.message(&summary.id))
                .filter(|message| crate::ops::demo_matches(&parsed.expr, message, self))
                .collect::<Vec<_>>();
            candidates.sort_by_key(|message| (message.summary.date, message.summary.id.clone()));
            candidates.truncate(usize::try_from(MAX_RUN_MESSAGES).unwrap_or(usize::MAX));
            for message in candidates {
                if stopped.contains(&message.summary.id) {
                    continue;
                }
                run.matched = run.matched.saturating_add(1);
                let mut state = self.demo_state(&message.summary.id);
                for action in &rule.actions {
                    match self.demo_action(account, &message, &mut state, action) {
                        Ok(applied) => {
                            if applied.changed {
                                run.changed = run.changed.saturating_add(1);
                            }
                            if let Some(value) = applied.value {
                                actions_log.push(RuleActionRecord {
                                    run_id: run_id.clone(),
                                    ordinal,
                                    rule_id: rule.id.clone(),
                                    message_id: message.summary.id.clone(),
                                    action: action.kind().to_owned(),
                                    value_json: value,
                                    changed: applied.changed,
                                });
                                ordinal = ordinal.saturating_add(1);
                            }
                        }
                        Err(error) => {
                            errors.push(error);
                            run.errors = run.errors.saturating_add(1);
                            break;
                        }
                    }
                }
                if rule.stop {
                    stopped.insert(message.summary.id.clone());
                }
            }
        }
        run.finished_at = Some(Utc::now().timestamp_millis());
        if scope == RuleRunScope::New {
            for id in &universe {
                self.demo_rules_applied.insert(id.clone());
            }
        }
        self.rule_run_actions.insert(run_id, actions_log);
        self.rule_runs.insert(0, run.clone());
        self.rule_runs.truncate(usize::try_from(MAX_KEPT_RUNS).unwrap_or(MAX_KEPT_RUNS as usize));
        self.bump_revision();
        Ok(RuleRunOutcome { run, errors })
    }

    fn demo_state(&self, message_id: &str) -> RuleMessageState {
        self.summary(message_id)
            .and_then(|summary| {
                let folder = self.folders().iter().find(|folder| folder.id == summary.folder_id)?;
                Some(RuleMessageState {
                    folder_id: summary.folder_id.clone(),
                    folder_kind: store_kind(folder.kind),
                    unread: summary.unread,
                    labels: summary.labels.clone(),
                })
            })
            .unwrap_or(RuleMessageState {
                folder_id: String::new(),
                folder_kind: postino_store::FolderKind::Custom,
                unread: false,
                labels: Vec::new(),
            })
    }

    fn demo_action(&mut self, account: &Account, message: &Message, state: &mut RuleMessageState, action: &RuleAction) -> Result<Applied, String> {
        let id = message.summary.id.clone();
        match action {
            RuleAction::Move { folder } => {
                let target = self
                    .folders()
                    .iter()
                    .find(|candidate| candidate.id == *folder && candidate.account_id == account.id)
                    .ok_or_else(|| format!("rule action: folder {folder:?} no longer exists"))?;
                if target.kind.is_virtual() || matches!(target.kind, FolderKind::Drafts | FolderKind::Sent) {
                    return Err(format!("rule action: folder {folder:?} is not a valid destination"));
                }
                if state.folder_id == *folder {
                    return Ok(Applied { changed: false, value: None });
                }
                let target_kind = target.kind;
                let from = state.folder_id.clone();
                self.move_message(&id, folder).map_err(|_| format!("rule action: message {id:?} could not be moved to {folder:?}"))?;
                state.folder_id = folder.clone();
                state.folder_kind = store_kind(target_kind);
                Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": folder, "after": state_json(state) }).to_string()) })
            }
            RuleAction::Label { label } => {
                if self.label(label).is_none() {
                    return Err(format!("rule action: label {label:?} no longer exists"));
                }
                if state.labels.iter().any(|candidate| candidate == label) {
                    return Ok(Applied { changed: false, value: None });
                }
                self.set_label(label, std::slice::from_ref(&id), true)
                    .map_err(|_| format!("rule action: label {label:?} could not be applied to {id:?}"))?;
                state.labels.push(label.clone());
                Ok(Applied { changed: true, value: Some(json!({ "label": label, "applied": true, "after": state_json(state) }).to_string()) })
            }
            RuleAction::Read | RuleAction::Unread => {
                let unread = matches!(action, RuleAction::Unread);
                if state.unread == unread {
                    return Ok(Applied { changed: false, value: None });
                }
                let from = state.unread;
                self.set_flags(&id, Some(unread), None).map_err(|_| format!("rule action: message {id:?} could not be flagged"))?;
                state.unread = unread;
                Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": unread, "after": state_json(state) }).to_string()) })
            }
            RuleAction::Trash => {
                let trash = self
                    .folders()
                    .iter()
                    .find(|folder| folder.account_id == account.id && folder.kind == FolderKind::Trash)
                    .map(|folder| folder.id.clone())
                    .ok_or_else(|| "rule action: the account has no Trash folder".to_owned())?;
                if state.folder_kind == postino_store::FolderKind::Trash {
                    return Ok(Applied { changed: false, value: None });
                }
                let from = state.folder_id.clone();
                self.move_message(&id, &trash).map_err(|_| format!("rule action: message {id:?} could not be moved to Trash"))?;
                state.folder_id = trash.clone();
                state.folder_kind = postino_store::FolderKind::Trash;
                Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": trash, "after": state_json(state) }).to_string()) })
            }
            RuleAction::Forward { .. } => Err("rule action: forward needs a local store and the P7 outbox".to_owned()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Undo and history
    // -----------------------------------------------------------------------------------------

    /// Reverse one run's reversible actions, newest first, only where the message is still in the
    /// state the action left it in. A forward is never undone; its outbox id is reported.
    pub fn undo_rule_run(&mut self, account: Option<&str>, id: &str) -> Result<RuleUndoOutcome, OpError> {
        let account = self.account_for(account, "rules_undo")?;
        let outcome = match self.store() {
            Some(store) => undo_store(store, &account.id, id)?,
            None => self.undo_demo(&account.id, id)?,
        };
        if self.is_persistent() {
            self.reload_mailbox()?;
        }
        Ok(outcome)
    }

    fn undo_demo(&mut self, account_id: &str, id: &str) -> Result<RuleUndoOutcome, OpError> {
        let run = self.rule_runs.iter_mut().find(|run| run.id == id && run.account_id == account_id).ok_or_else(|| unknown_run(id))?;
        if run.undone {
            return Err(OpError::InvalidArgs(format!("rule run {id:?} was already undone")));
        }
        let actions = self.rule_run_actions.get(id).cloned().unwrap_or_default();
        let expected = expected_states(&actions);
        let mut changed_messages = BTreeSet::new();
        for (message_id, expected) in &expected {
            let state = self.demo_state(message_id);
            if !state_matches(&state, expected) {
                changed_messages.insert(message_id.clone());
            }
        }
        let mut restored = 0u32;
        let mut skipped = 0u32;
        let mut forwards = Vec::new();
        for action in actions.iter().rev() {
            let value: Value = serde_json::from_str(&action.value_json).unwrap_or(Value::Null);
            match action.action.as_str() {
                "forward" => {
                    if let Some(outbox) = value.get("outbox").and_then(Value::as_str) {
                        forwards.push(outbox.to_owned());
                    }
                }
                _ if changed_messages.contains(&action.message_id) => skipped = skipped.saturating_add(1),
                "move" | "trash" => {
                    let (Some(from), Some(to)) = (value.get("from").and_then(Value::as_str), value.get("to").and_then(Value::as_str)) else {
                        skipped = skipped.saturating_add(1);
                        continue;
                    };
                    if self.summary(&action.message_id).is_some_and(|summary| summary.folder_id == to) {
                        match self.move_message(&action.message_id, from) {
                            Ok(_) => restored = restored.saturating_add(1),
                            Err(_) => skipped = skipped.saturating_add(1),
                        }
                    } else {
                        skipped = skipped.saturating_add(1);
                    }
                }
                "label" => {
                    let Some(label) = value.get("label").and_then(Value::as_str) else {
                        skipped = skipped.saturating_add(1);
                        continue;
                    };
                    if self.label(label).is_some()
                        && self.summary(&action.message_id).is_some_and(|summary| summary.labels.iter().any(|candidate| candidate == label))
                    {
                        match self.set_label(label, std::slice::from_ref(&action.message_id), false) {
                            Ok(_) => restored = restored.saturating_add(1),
                            Err(_) => skipped = skipped.saturating_add(1),
                        }
                    } else {
                        skipped = skipped.saturating_add(1);
                    }
                }
                "read" | "unread" => {
                    let (Some(from), Some(to)) = (value.get("from").and_then(Value::as_bool), value.get("to").and_then(Value::as_bool)) else {
                        skipped = skipped.saturating_add(1);
                        continue;
                    };
                    if self.summary(&action.message_id).is_some_and(|summary| summary.unread == to) {
                        match self.set_flags(&action.message_id, Some(from), None) {
                            Ok(_) => restored = restored.saturating_add(1),
                            Err(_) => skipped = skipped.saturating_add(1),
                        }
                    } else {
                        skipped = skipped.saturating_add(1);
                    }
                }
                _ => skipped = skipped.saturating_add(1),
            }
        }
        if let Some(run) = self.rule_runs.iter_mut().find(|run| run.id == id) {
            run.undone = true;
        }
        let run = self.rule_runs.iter().find(|run| run.id == id).cloned().ok_or_else(|| unknown_run(id))?;
        self.bump_revision();
        Ok(RuleUndoOutcome { run, restored, skipped, forwards })
    }

    /// The newest runs of one account, at most 20.
    pub fn rule_runs(&self, account: Option<&str>, limit: Option<u32>) -> Result<Vec<RuleRun>, OpError> {
        let account = self.account_for(account, "rules_runs")?;
        let limit = limit.unwrap_or(20).clamp(1, MAX_KEPT_RUNS);
        match self.store() {
            Some(store) => {
                Ok(store.list_rule_runs(&account.id, limit).map_err(store_map::read_error)?.into_iter().map(rule_run_from_store).collect())
            }
            None => {
                Ok(self.rule_runs.iter().filter(|run| run.account_id == account.id).take(usize::try_from(limit).unwrap_or(20)).cloned().collect())
            }
        }
    }
}

fn rule_limit_error(account_id: &str) -> OpError {
    OpError::InvalidArgs(format!("account {account_id:?} already has {MAX_RULES} rules; delete one before adding another"))
}

fn unknown_account(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown account {id:?}; use an id returned by accounts_list"))
}

fn unknown_rule(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown rule {id:?}; use an id returned by rules_list"))
}

fn unknown_run(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown rule run {id:?}; use an id returned by rules_runs"))
}

/// The post-action state of a message, as ids and booleans only. Undo compares the live state with
/// this snapshot before reversing anything: a message the user moved, flagged or labelled after the
/// run is left alone. The journal never carries a subject, an address or a body.
fn state_json(state: &RuleMessageState) -> Value {
    json!({ "folder": state.folder_id, "unread": state.unread, "labels": state.labels })
}

/// The last post-action state journaled for each message. The last action for a message is its
/// final state; a forward that changed nothing still carries the same snapshot.
fn expected_states(actions: &[RuleActionRecord]) -> HashMap<String, Value> {
    let mut expected = HashMap::new();
    for action in actions {
        if let Ok(value) = serde_json::from_str::<Value>(&action.value_json)
            && let Some(after) = value.get("after")
        {
            expected.insert(action.message_id.clone(), after.clone());
        }
    }
    expected
}

fn state_matches(state: &RuleMessageState, expected: &Value) -> bool {
    let folder = expected.get("folder").and_then(Value::as_str);
    let unread = expected.get("unread").and_then(Value::as_bool);
    let labels = expected.get("labels").and_then(Value::as_array).map(|labels| labels.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>());
    folder == Some(state.folder_id.as_str())
        && unread == Some(state.unread)
        && labels.is_some_and(|labels| labels == state.labels.iter().map(String::as_str).collect::<BTreeSet<_>>())
}

fn store_kind(kind: FolderKind) -> postino_store::FolderKind {
    match kind {
        FolderKind::Inbox => postino_store::FolderKind::Inbox,
        FolderKind::Starred => postino_store::FolderKind::Starred,
        FolderKind::Sent => postino_store::FolderKind::Sent,
        FolderKind::Drafts => postino_store::FolderKind::Drafts,
        FolderKind::Archive => postino_store::FolderKind::Archive,
        FolderKind::Spam => postino_store::FolderKind::Spam,
        FolderKind::Trash => postino_store::FolderKind::Trash,
        FolderKind::Custom => postino_store::FolderKind::Custom,
    }
}

fn validate_rule_name(name: &str) -> Result<String, OpError> {
    let name = name.trim();
    let characters = name.chars().count();
    if characters == 0 {
        return Err(OpError::InvalidArgs("a rule name must have 1-48 characters after trimming".to_owned()));
    }
    if characters > 48 {
        return Err(OpError::InvalidArgs("a rule name must have 1-48 characters after trimming".to_owned()));
    }
    if name.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("a rule name must not contain control characters".to_owned()));
    }
    if name.chars().any(crate::engine::is_bidi_or_format_control) {
        return Err(OpError::InvalidArgs("a rule name must not contain bidi or format control characters".to_owned()));
    }
    Ok(name.to_owned())
}

fn validate_rule_query(engine: &Engine, account_id: &str, query: &str) -> Result<String, OpError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(OpError::InvalidArgs("a rule needs a non-empty search query".to_owned()));
    }
    crate::ops::validate_search_query(engine, query, Some(account_id))?;
    Ok(query.to_owned())
}

fn validate_rule_actions(engine: &Engine, account_id: &str, actions: &[RuleAction]) -> Result<Vec<RuleAction>, OpError> {
    if actions.is_empty() {
        return Err(OpError::InvalidArgs("a rule needs 1-5 actions".to_owned()));
    }
    if actions.len() > MAX_RULE_ACTIONS {
        return Err(OpError::InvalidArgs(format!("a rule accepts at most {MAX_RULE_ACTIONS} actions")));
    }
    let mut kinds = BTreeSet::new();
    let mut normalized = Vec::with_capacity(actions.len());
    for action in actions {
        let kind = action.kind();
        if !kinds.insert(kind) {
            return Err(OpError::InvalidArgs(format!("a rule may carry only one {kind} action")));
        }
        normalized.push(match action {
            RuleAction::Move { folder } => {
                let folder = engine
                    .folders()
                    .iter()
                    .find(|candidate| candidate.id == *folder && candidate.account_id == account_id)
                    .ok_or_else(|| OpError::InvalidArgs(format!("unknown folder {folder:?} for this rule's account")))?;
                if folder.kind.is_virtual() || matches!(folder.kind, FolderKind::Drafts | FolderKind::Sent) {
                    return Err(OpError::InvalidArgs(format!(
                        "folder {:?} cannot be a rule destination: a rule never edits a draft or a Sent copy",
                        folder.id
                    )));
                }
                RuleAction::Move { folder: folder.id.clone() }
            }
            RuleAction::Label { label } => {
                if engine.label(label).is_none() {
                    return Err(OpError::InvalidArgs(format!("unknown label {label:?}; use an id returned by labels_list")));
                }
                RuleAction::Label { label: label.clone() }
            }
            RuleAction::Read => RuleAction::Read,
            RuleAction::Unread => RuleAction::Unread,
            RuleAction::Trash => RuleAction::Trash,
            RuleAction::Forward { to } => {
                if to.is_empty() || to.len() > MAX_FORWARD_RECIPIENTS {
                    return Err(OpError::InvalidArgs(format!("a forward action needs 1-{MAX_FORWARD_RECIPIENTS} recipients")));
                }
                validate_addresses("forward", to)?;
                let mut seen = BTreeSet::new();
                for address in to {
                    if !seen.insert(address.email.trim().to_ascii_lowercase()) {
                        return Err(OpError::InvalidArgs("a forward recipient is repeated; every address must be distinct".to_owned()));
                    }
                }
                RuleAction::Forward { to: to.clone() }
            }
        });
    }
    if kinds.contains("read") && kinds.contains("unread") {
        return Err(OpError::InvalidArgs("a rule cannot mark a message both read and unread".to_owned()));
    }
    Ok(normalized)
}

fn preview_effects(actions: &[RuleAction], message: &MessageSummary, folder_kind: Option<FolderKind>, guard: RuleGuard) -> Vec<RulePreviewEffect> {
    let mut folder_id = message.folder_id.clone();
    let mut unread = message.unread;
    let mut labels = message.labels.clone();
    let mut effects = Vec::with_capacity(actions.len());
    for action in actions {
        let status = match action {
            RuleAction::Move { folder } => {
                if folder_id == *folder {
                    RuleEffectStatus::NoOp
                } else {
                    folder_id = folder.clone();
                    RuleEffectStatus::Apply
                }
            }
            RuleAction::Label { label } => {
                if labels.iter().any(|candidate| candidate == label) {
                    RuleEffectStatus::NoOp
                } else {
                    labels.push(label.clone());
                    RuleEffectStatus::Apply
                }
            }
            RuleAction::Read | RuleAction::Unread => {
                let wanted = matches!(action, RuleAction::Unread);
                if unread == wanted {
                    RuleEffectStatus::NoOp
                } else {
                    unread = wanted;
                    RuleEffectStatus::Apply
                }
            }
            RuleAction::Trash => {
                if folder_kind == Some(FolderKind::Trash) {
                    RuleEffectStatus::NoOp
                } else {
                    RuleEffectStatus::Apply
                }
            }
            RuleAction::Forward { .. } => {
                if guard.any() {
                    RuleEffectStatus::Skip
                } else {
                    RuleEffectStatus::Apply
                }
            }
        };
        effects.push(RulePreviewEffect { action: action.clone(), status });
    }
    effects
}

fn run_store(engine: &Engine, store: &Store, account: &Account, rules: &[Rule], scope: RuleRunScope) -> Result<RuleRunOutcome, OpError> {
    let started = Utc::now().timestamp_millis();
    let run_id = store.allocate_rule_run_id().map_err(store_map::write_error)?;
    let mode = match scope {
        RuleRunScope::New => "new",
        RuleRunScope::All => "all",
    };
    let no_rules = rules.is_empty();
    let mut run = RuleRun {
        id: run_id.clone(),
        account_id: account.id.clone(),
        started_at: started,
        finished_at: None,
        mode: scope,
        rule_count: u32::try_from(rules.len()).unwrap_or(u32::MAX),
        matched: 0,
        changed: 0,
        errors: 0,
        undone: false,
    };
    store
        .create_rule_run(&StoredRuleRun {
            id: run.id.clone(),
            account_id: run.account_id.clone(),
            started_at: run.started_at,
            finished_at: None,
            mode: mode.to_owned(),
            rule_count: run.rule_count,
            matched: 0,
            changed: 0,
            errors: 0,
            undone: false,
        })
        .map_err(store_map::write_error)?;
    if no_rules {
        run.finished_at = Some(Utc::now().timestamp_millis());
        store.finish_rule_run(&run.id, run.finished_at.unwrap_or(run.started_at), 0, 0, 0).map_err(store_map::write_error)?;
        store.prune_rule_runs(&account.id, MAX_KEPT_RUNS).map_err(store_map::write_error)?;
        return Ok(RuleRunOutcome { run, errors: Vec::new() });
    }
    // A `new` run fixes one bounded universe of new received mail, then every rule sees it in
    // order. An `all` run instead gives each rule its own 2 000-candidate page: a rule created today
    // must not be starved by an older rule whose own scope starts years earlier.
    let universe = if scope == RuleRunScope::New {
        let since = rules.iter().map(|rule| rule.created_at).min().unwrap_or(started);
        Some(
            store
                .rule_candidates(&RuleCandidateFilter { account_id: account.id.clone(), expr: None, since, new_only: true, limit: MAX_RUN_MESSAGES })
                .map_err(store_map::read_error)?
                .messages
                .into_iter()
                .map(|message| message.id)
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };
    let mut errors = Vec::new();
    let mut stopped = BTreeSet::new();
    let mut ordinal = 1u32;
    let mut forwards = 0u32;
    for rule in rules {
        let parsed = match crate::search::parse(&rule.query) {
            Ok(parsed) => parsed,
            Err(error) => {
                errors.push(format!("rule {:?}: the stored query is not readable ({} at position {})", rule.id, error.code, error.position));
                run.errors = run.errors.saturating_add(1);
                continue;
            }
        };
        let expr = match crate::ops::translate_search(&parsed.expr, engine, Some(&account.id)) {
            Ok(expr) => expr,
            Err(error) => {
                errors.push(format!("rule {:?}: the stored query no longer resolves ({error})", rule.id));
                run.errors = run.errors.saturating_add(1);
                continue;
            }
        };
        let page = store
            .rule_candidates(&RuleCandidateFilter {
                account_id: account.id.clone(),
                expr: Some(expr),
                since: rule.created_at,
                new_only: scope == RuleRunScope::New,
                limit: MAX_RUN_MESSAGES,
            })
            .map_err(store_map::read_error)?;
        for stored in page.messages {
            let candidate = crate::store_map::summary(stored);
            if universe.as_ref().is_some_and(|universe| !universe.contains(&candidate.id)) || stopped.contains(&candidate.id) {
                continue;
            }
            run.matched = run.matched.saturating_add(1);
            let Some(mut state) = store.rule_message_state(&candidate.id).map_err(store_map::read_error)? else {
                errors.push(format!("rule {:?}: message {:?} disappeared before its actions ran", rule.id, candidate.id));
                run.errors = run.errors.saturating_add(1);
                continue;
            };
            for action in &rule.actions {
                match apply_store_action(engine, store, account, rule, &candidate, &mut state, action, &mut forwards) {
                    Ok(applied) => {
                        if applied.changed {
                            run.changed = run.changed.saturating_add(1);
                        }
                        if let Some(value) = applied.value {
                            store
                                .append_rule_action(&RuleActionRecord {
                                    run_id: run_id.clone(),
                                    ordinal,
                                    rule_id: rule.id.clone(),
                                    message_id: candidate.id.clone(),
                                    action: action.kind().to_owned(),
                                    value_json: value,
                                    changed: applied.changed,
                                })
                                .map_err(store_map::write_error)?;
                            ordinal = ordinal.saturating_add(1);
                        }
                    }
                    Err(error) => {
                        errors.push(error);
                        run.errors = run.errors.saturating_add(1);
                        break;
                    }
                }
            }
            if rule.stop {
                stopped.insert(candidate.id.clone());
            }
        }
    }
    run.finished_at = Some(Utc::now().timestamp_millis());
    store
        .finish_rule_run(&run.id, run.finished_at.unwrap_or(run.started_at), run.matched, run.changed, run.errors)
        .map_err(store_map::write_error)?;
    store.prune_rule_runs(&account.id, MAX_KEPT_RUNS).map_err(store_map::write_error)?;
    if let Some(universe) = &universe {
        let ids = universe.iter().cloned().collect::<Vec<_>>();
        store.mark_rules_applied(&ids, started).map_err(store_map::write_error)?;
    }
    Ok(RuleRunOutcome { run, errors })
}

#[allow(clippy::too_many_arguments)]
fn apply_store_action(
    engine: &Engine,
    store: &Store,
    account: &Account,
    rule: &Rule,
    message: &MessageSummary,
    state: &mut RuleMessageState,
    action: &RuleAction,
    forwards: &mut u32,
) -> Result<Applied, String> {
    let id = &message.id;
    match action {
        RuleAction::Move { folder } => {
            let target = engine
                .folders()
                .iter()
                .find(|candidate| candidate.id == *folder && candidate.account_id == account.id)
                .ok_or_else(|| format!("rule {:?}: folder {folder:?} no longer exists", rule.id))?;
            if target.kind.is_virtual() || matches!(target.kind, FolderKind::Drafts | FolderKind::Sent) {
                return Err(format!("rule {:?}: folder {folder:?} is not a valid destination", rule.id));
            }
            if state.folder_id == *folder {
                return Ok(Applied { changed: false, value: None });
            }
            let from = state.folder_id.clone();
            store.move_messages(std::slice::from_ref(id), folder).map_err(|_| format!("rule {:?}: message {id:?} could not be moved", rule.id))?;
            state.folder_id = folder.clone();
            state.folder_kind = store_kind(target.kind);
            Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": folder, "after": state_json(state) }).to_string()) })
        }
        RuleAction::Label { label } => {
            if engine.label(label).is_none() {
                return Err(format!("rule {:?}: label {label:?} no longer exists", rule.id));
            }
            if state.labels.iter().any(|candidate| candidate == label) {
                return Ok(Applied { changed: false, value: None });
            }
            store.apply_label(label, std::slice::from_ref(id)).map_err(|_| format!("rule {:?}: label {label:?} could not be applied", rule.id))?;
            state.labels.push(label.clone());
            Ok(Applied { changed: true, value: Some(json!({ "label": label, "applied": true, "after": state_json(state) }).to_string()) })
        }
        RuleAction::Read | RuleAction::Unread => {
            let unread = matches!(action, RuleAction::Unread);
            if state.unread == unread {
                return Ok(Applied { changed: false, value: None });
            }
            let from = state.unread;
            store
                .set_flags(std::slice::from_ref(id), postino_store::FlagUpdate { unread: Some(unread), starred: None })
                .map_err(|_| format!("rule {:?}: message {id:?} could not be flagged", rule.id))?;
            state.unread = unread;
            Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": unread, "after": state_json(state) }).to_string()) })
        }
        RuleAction::Trash => {
            let trash = engine
                .folders()
                .iter()
                .find(|folder| folder.account_id == account.id && folder.kind == FolderKind::Trash)
                .map(|folder| folder.id.clone())
                .ok_or_else(|| format!("rule {:?}: the account has no Trash folder", rule.id))?;
            if state.folder_kind == postino_store::FolderKind::Trash {
                return Ok(Applied { changed: false, value: None });
            }
            let from = state.folder_id.clone();
            store
                .move_messages(std::slice::from_ref(id), &trash)
                .map_err(|_| format!("rule {:?}: message {id:?} could not be moved to Trash", rule.id))?;
            state.folder_id = trash.clone();
            state.folder_kind = postino_store::FolderKind::Trash;
            Ok(Applied { changed: true, value: Some(json!({ "from": from, "to": trash, "after": state_json(state) }).to_string()) })
        }
        RuleAction::Forward { to } => {
            let guard = store.rule_guard(id).map_err(|_| format!("rule {:?}: the loop guard could not be read", rule.id))?;
            if guard.any() {
                // A guarded message is not a forward: it must not consume the run's 50-forward
                // budget or stop the next message.
                return Ok(Applied {
                    changed: false,
                    value: Some(json!({ "outbox": null, "skipped": true, "after": state_json(state) }).to_string()),
                });
            }
            *forwards = forwards.saturating_add(1);
            if *forwards > MAX_RUN_FORWARDS {
                return Err(format!("rule {:?}: the run limit of {MAX_RUN_FORWARDS} forwards was reached", rule.id));
            }
            match queue_rule_forward(engine, store, account, &rule.id, id, to) {
                Ok(outcome) => {
                    Ok(Applied { changed: outcome.created, value: Some(json!({ "outbox": outcome.outbox, "after": state_json(state) }).to_string()) })
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Queue one rule forward through the P7 outbox. It builds a standalone submission, never a draft:
/// the row has no local draft to move, and a later sync caches the server Sent copy.
fn queue_rule_forward(
    engine: &Engine,
    store: &Store,
    account: &Account,
    rule_id: &str,
    message_id: &str,
    to: &[Address],
) -> Result<ForwardOutcome, String> {
    let key = format!("rule:{rule_id}:{message_id}");
    if let Some(existing) = store.outbox_by_key(&key).map_err(|_| "rule forward: the outbox could not be read".to_owned())? {
        return Ok(ForwardOutcome { outbox: existing.id, created: false });
    }
    let server = store
        .get_account_server(&account.id)
        .map_err(|_| format!("rule {rule_id:?}: the account configuration could not be read"))?
        .ok_or_else(|| format!("rule {rule_id:?}: the account has no submission endpoint"))?;
    if server.smtp.is_none() {
        return Err(format!("rule {rule_id:?}: the account has no SMTP endpoint, so it can read but not forward"));
    }
    if to.is_empty() || to.len() > MAX_FORWARD_RECIPIENTS {
        return Err(format!("rule {rule_id:?}: a forward action needs 1-{MAX_FORWARD_RECIPIENTS} recipients"));
    }
    let message = engine
        .message_result(message_id)
        .map_err(|_| format!("rule {rule_id:?}: message {message_id:?} could not be read for forwarding"))?
        .ok_or_else(|| format!("rule {rule_id:?}: message {message_id:?} no longer exists"))?;
    let prefill = crate::reply::compose_reply(account, &message, None, &[], crate::reply::ReplyMode::Forward);
    let mut attachments = Vec::with_capacity(message.attachments.len());
    let mut total = 0u64;
    if message.attachments.len() > crate::engine::MAX_DRAFT_ATTACHMENTS {
        return Err(format!("rule {rule_id:?}: the message has too many attachments to forward"));
    }
    for attachment in &message.attachments {
        let bytes = engine
            .attachment_bytes(&attachment.id)
            .map_err(|_| format!("rule {rule_id:?}: an attachment of message {message_id:?} has no stored bytes"))?;
        total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if total > MAX_ATTACHMENT_TOTAL_BYTES {
            return Err(format!("rule {rule_id:?}: the forwarded attachments exceed the 25 MiB limit"));
        }
        attachments.push(postino_sync::SubmissionAttachment {
            filename: attachment.filename.clone(),
            mime: attachment.mime.clone(),
            data: bytes,
            content_id: None,
        });
    }
    let submission = postino_sync::Submission {
        from: postino_sync::SubmissionMailbox { name: Some(account.name.clone()), address: account.email.clone() },
        to: to
            .iter()
            .map(|address| postino_sync::SubmissionMailbox {
                name: (!address.name.trim().is_empty()).then(|| address.name.clone()),
                address: address.email.clone(),
            })
            .collect(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: prefill.subject,
        date: Utc::now(),
        text: prefill.body,
        html: None,
        attachments,
        in_reply_to: None,
        references: Vec::new(),
        message_id: None,
        extra_headers: vec![("Auto-Submitted".to_owned(), "auto-forwarded".to_owned()), ("X-Postino-Rule".to_owned(), rule_id.to_owned())],
    };
    let serialized = postino_sync::serialize_submission(&submission).map_err(|_| format!("rule {rule_id:?}: the forward could not be serialized"))?;
    if serialized.raw.len() as u64 > crate::outbox::MAX_SUBMISSION_BYTES {
        return Err(format!("rule {rule_id:?}: the forwarded message is larger than the submission limit"));
    }
    let raw_hash = store.blobs().put(&serialized.raw).map_err(|_| format!("rule {rule_id:?}: the forward bytes could not be stored"))?;
    let digest = blake3::hash(key.as_bytes()).to_hex();
    let id = format!("ob_{}", digest.as_str().get(..16).unwrap_or(digest.as_str()));
    let now = Utc::now().timestamp_millis();
    let envelope = OutboxEnvelope {
        from: serialized.envelope.from.clone(),
        recipients: serialized.envelope.recipients.clone(),
        subject: submission.subject.clone(),
        message_id: serialized.message_id.clone(),
        draft_id: None,
        draft_revision: None,
    };
    let envelope_json = envelope.encode().map_err(|_| format!("rule {rule_id:?}: the forward envelope could not be encoded"))?;
    let entry = postino_store::OutboxEntry {
        id: id.clone(),
        account_id: account.id.clone(),
        draft_id: None,
        idempotency_key: key.clone(),
        created_at: now,
        send_at: now,
        state: postino_store::OutboxState::Queued,
        attempts: 0,
        append_attempts: 0,
        last_error: None,
        raw_blob_hash: raw_hash,
        envelope_json,
        lease_at: None,
    };
    match store.queue_outbox(&entry) {
        Ok(row) => Ok(ForwardOutcome { outbox: row.id, created: true }),
        Err(StoreError::OutboxAlreadyActive { .. }) => match store.outbox_by_key(&key) {
            Ok(Some(row)) => Ok(ForwardOutcome { outbox: row.id, created: false }),
            _ => Err(format!("rule {rule_id:?}: the forward was queued concurrently and its row is not readable")),
        },
        Err(_) => Err(format!("rule {rule_id:?}: the forward could not be queued")),
    }
}

fn undo_store(store: &Store, account_id: &str, id: &str) -> Result<RuleUndoOutcome, OpError> {
    let mut run = store.get_rule_run(id).map_err(store_map::read_error)?.ok_or_else(|| unknown_run(id))?;
    if run.account_id != account_id {
        return Err(unknown_run(id));
    }
    if run.undone {
        return Err(OpError::InvalidArgs(format!("rule run {id:?} was already undone")));
    }
    let actions = store.rule_run_actions(id).map_err(store_map::read_error)?;
    let expected = expected_states(&actions);
    let mut changed_messages = BTreeSet::new();
    for (message_id, expected) in &expected {
        let state = store.rule_message_state(message_id).map_err(store_map::read_error)?;
        if state.is_none_or(|state| !state_matches(&state, expected)) {
            changed_messages.insert(message_id.clone());
        }
    }
    let mut restored = 0u32;
    let mut skipped = 0u32;
    let mut forwards = Vec::new();
    for action in actions.iter().rev() {
        let value: Value = serde_json::from_str(&action.value_json).unwrap_or(Value::Null);
        match action.action.as_str() {
            "forward" => {
                if let Some(outbox) = value.get("outbox").and_then(Value::as_str) {
                    forwards.push(outbox.to_owned());
                }
            }
            _ if changed_messages.contains(&action.message_id) => skipped = skipped.saturating_add(1),
            "move" | "trash" => {
                let (Some(from), Some(to)) = (value.get("from").and_then(Value::as_str), value.get("to").and_then(Value::as_str)) else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let state = store.rule_message_state(&action.message_id).map_err(store_map::read_error)?;
                if state.is_some_and(|state| state.folder_id == to) {
                    match store.move_messages(std::slice::from_ref(&action.message_id), from) {
                        Ok(_) => restored = restored.saturating_add(1),
                        Err(_) => skipped = skipped.saturating_add(1),
                    }
                } else {
                    skipped = skipped.saturating_add(1);
                }
            }
            "label" => {
                let Some(label) = value.get("label").and_then(Value::as_str) else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let state = store.rule_message_state(&action.message_id).map_err(store_map::read_error)?;
                if state.is_some_and(|state| state.labels.iter().any(|candidate| candidate == label)) {
                    match store.remove_label(label, std::slice::from_ref(&action.message_id)) {
                        Ok(_) => restored = restored.saturating_add(1),
                        Err(_) => skipped = skipped.saturating_add(1),
                    }
                } else {
                    skipped = skipped.saturating_add(1);
                }
            }
            "read" | "unread" => {
                let (Some(from), Some(to)) = (value.get("from").and_then(Value::as_bool), value.get("to").and_then(Value::as_bool)) else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let state = store.rule_message_state(&action.message_id).map_err(store_map::read_error)?;
                if state.is_some_and(|state| state.unread == to) {
                    match store.set_flags(std::slice::from_ref(&action.message_id), postino_store::FlagUpdate { unread: Some(from), starred: None }) {
                        Ok(_) => restored = restored.saturating_add(1),
                        Err(_) => skipped = skipped.saturating_add(1),
                    }
                } else {
                    skipped = skipped.saturating_add(1);
                }
            }
            _ => skipped = skipped.saturating_add(1),
        }
    }
    if !store.mark_rule_run_undone(id, Utc::now().timestamp_millis()).map_err(store_map::write_error)? {
        return Err(OpError::InvalidArgs(format!("rule run {id:?} was already undone")));
    }
    run.undone = true;
    Ok(RuleUndoOutcome { run: rule_run_from_store(run), restored, skipped, forwards })
}

fn rule_run_from_store(run: StoredRuleRun) -> RuleRun {
    RuleRun {
        id: run.id,
        account_id: run.account_id,
        started_at: run.started_at,
        finished_at: run.finished_at,
        mode: if run.mode == "all" { RuleRunScope::All } else { RuleRunScope::New },
        rule_count: run.rule_count,
        matched: run.matched,
        changed: run.changed,
        errors: run.errors,
        undone: run.undone,
    }
}
