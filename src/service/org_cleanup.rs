// Copyright 2026 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use config::spawn_pausable_job;
use infra::{dist_lock, table::org_cleanup_tasks};

const LOCK_KEY: &str = "/org_cleanup/worker_lock";
const MAX_ATTEMPTS: i32 = 10;
const POLL_INTERVAL_SECS: u64 = 30;

const ORDER_DELETE_STREAMS: i32 = 100;
const ORDER_DELETE_STREAM_ITEM: i32 = 150;
const ORDER_DELETE_FILE_LIST: i32 = 200;
const ORDER_DELETE_DB_RESOURCES: i32 = 300;
const ORDER_DELETE_SCHEDULER_TRIGGERS: i32 = 400;
const ORDER_DELETE_K8S_RESOURCES: i32 = 500;
const ORDER_DELETE_USERS: i32 = 600;
const ORDER_DELETE_OFGA: i32 = 700;
const ORDER_DELETE_CLOUD_BILLING: i32 = 800;
const ORDER_DELETE_ORG_RECORD: i32 = 900;

pub fn fixed_steps(org_id: &str, org_name: &str) -> Vec<org_cleanup_tasks::NewCleanupTask> {
    vec![
        ("delete_streams", ORDER_DELETE_STREAMS),
        ("delete_file_list", ORDER_DELETE_FILE_LIST),
        ("delete_db_resources", ORDER_DELETE_DB_RESOURCES),
        ("delete_scheduler_triggers", ORDER_DELETE_SCHEDULER_TRIGGERS),
        ("delete_k8s_resources", ORDER_DELETE_K8S_RESOURCES),
        ("delete_users", ORDER_DELETE_USERS),
        ("delete_ofga", ORDER_DELETE_OFGA),
        ("delete_cloud_billing", ORDER_DELETE_CLOUD_BILLING),
        ("delete_org_record", ORDER_DELETE_ORG_RECORD),
    ]
    .into_iter()
    .map(|(step, order)| org_cleanup_tasks::NewCleanupTask {
        org_id: org_id.to_string(),
        org_name: org_name.to_string(),
        step: step.to_string(),
        step_order: order,
    })
    .collect()
}

pub async fn run() -> Result<(), anyhow::Error> {
    spawn_pausable_job!("org_cleanup_worker", POLL_INTERVAL_SECS, {
        run_once().await;
    });
    Ok(())
}

pub async fn run_retention_purge() -> Result<(), anyhow::Error> {
    let thirty_days_micros: i64 = 30 * 24 * 3600 * 1_000_000;
    spawn_pausable_job!("org_cleanup_retention", 3600u64, {
        let cutoff = config::utils::time::now_micros() - thirty_days_micros;
        match org_cleanup_tasks::purge_done_before(cutoff).await {
            Ok(n) if n > 0 => log::info!("[org_cleanup] purged {n} done tasks"),
            Err(e) => log::error!("[org_cleanup] purge error: {e}"),
            _ => {}
        }
    });
    Ok(())
}

async fn run_once() {
    let locker = match dist_lock::lock(LOCK_KEY, 0).await {
        Ok(l) => l,
        Err(e) => {
            log::debug!("[org_cleanup] failed to acquire lock: {e}");
            return;
        }
    };

    let tasks = match org_cleanup_tasks::list_pending(MAX_ATTEMPTS).await {
        Ok(t) => t,
        Err(e) => {
            log::error!("[org_cleanup] failed to list pending tasks: {e}");
            let _ = dist_lock::unlock(&locker).await;
            return;
        }
    };

    if let Err(e) = dist_lock::unlock(&locker).await {
        log::error!("[org_cleanup] failed to release lock: {e}");
    }

    let mut by_org: std::collections::HashMap<String, Vec<org_cleanup_tasks::CleanupTask>> =
        std::collections::HashMap::new();
    for task in tasks {
        by_org.entry(task.org_id.clone()).or_default().push(task);
    }

    let futures: Vec<_> = by_org
        .into_values()
        .map(|mut org_tasks| {
            org_tasks.sort_by_key(|t| t.step_order);
            tokio::spawn(process_org_tasks(org_tasks))
        })
        .collect();

    for f in futures {
        if let Err(e) = f.await {
            log::error!("[org_cleanup] task panic: {e}");
        }
    }
}

async fn process_org_tasks(tasks: Vec<org_cleanup_tasks::CleanupTask>) {
    for task in &tasks {
        let predecessors_done =
            match org_cleanup_tasks::list_by_org_status(&task.org_id, None).await {
                Ok(all) => all
                    .iter()
                    .filter(|t| t.step_order < task.step_order)
                    .all(|t| t.status == "done"),
                Err(e) => {
                    log::error!(
                        "[org_cleanup] cannot check predecessors for {}: {e}",
                        task.org_id
                    );
                    return;
                }
            };

        if !predecessors_done {
            log::debug!(
                "[org_cleanup] org={} step={} waiting for predecessors",
                task.org_id,
                task.step
            );
            continue;
        }

        if task.status == "failed" && task.attempts >= MAX_ATTEMPTS {
            log::error!(
                "[org_cleanup] org={} step={} permanently failed after {} attempts",
                task.org_id,
                task.step,
                task.attempts
            );
            emit_failed_alert(&task.org_id, &task.step).await;
            continue;
        }

        match org_cleanup_tasks::mark_running(&task.id).await {
            Ok(true) => {}
            Ok(false) => {
                log::debug!(
                    "[org_cleanup] org={} step={} lost CAS race",
                    task.org_id,
                    task.step
                );
                continue;
            }
            Err(e) => {
                log::error!("[org_cleanup] mark_running error: {e}");
                continue;
            }
        }

        log::info!(
            "[org_cleanup] org={} step={} attempt={}",
            task.org_id,
            task.step,
            task.attempts + 1
        );

        let result = execute_step(&task.org_id, &task.org_name, &task.step).await;

        match result {
            Ok(()) => {
                log::info!("[org_cleanup] org={} step={} done", task.org_id, task.step);
                let _ = org_cleanup_tasks::mark_done(&task.id).await;
            }
            Err(e) => {
                log::error!(
                    "[org_cleanup] org={} step={} attempt={} error={e}",
                    task.org_id,
                    task.step,
                    task.attempts + 1
                );
                let _ = org_cleanup_tasks::mark_failed(&task.id, &e.to_string()).await;
            }
        }
    }
}

async fn emit_failed_alert(org_id: &str, _step: &str) {
    #[cfg(feature = "cloud")]
    {
        use crate::service::self_reporting::cloud_events::{
            CloudEvent, EventType, enqueue_cloud_event,
        };
        enqueue_cloud_event(CloudEvent {
            event: EventType::OrgCleanupFailed,
            org_id: org_id.to_string(),
            org_name: org_id.to_string(),
            org_type: String::new(),
            user: None,
            subscription_type: None,
            stream_name: None,
        })
        .await;
    }
    #[cfg(not(feature = "cloud"))]
    {
        log::error!(
            "[org_cleanup] org={org_id} permanently failed (alert not available in this build)"
        );
    }
}

async fn execute_step(org_id: &str, org_name: &str, step: &str) -> Result<(), anyhow::Error> {
    if step == "delete_streams" {
        step_delete_streams(org_id, org_name).await
    } else if let Some(rest) = step.strip_prefix("delete_stream:") {
        step_delete_stream(org_id, rest).await
    } else if step == "delete_file_list" {
        step_delete_file_list(org_id).await
    } else if step == "delete_db_resources" {
        step_delete_db_resources(org_id).await
    } else if step == "delete_scheduler_triggers" {
        step_delete_scheduler_triggers(org_id).await
    } else if step == "delete_k8s_resources" {
        step_delete_k8s_resources(org_id).await
    } else if step == "delete_users" {
        step_delete_users(org_id).await
    } else if step == "delete_ofga" {
        step_delete_ofga(org_id).await
    } else if step == "delete_cloud_billing" {
        step_delete_cloud_billing(org_id).await
    } else if step == "delete_org_record" {
        step_delete_org_record(org_id).await
    } else {
        Err(anyhow::anyhow!("unknown step: {step}"))
    }
}

async fn step_delete_streams(org_id: &str, org_name: &str) -> Result<(), anyhow::Error> {
    let streams = crate::service::db::schema::list(org_id, None, false).await?;

    // Enqueue one sub-task per stream
    let sub_tasks: Vec<org_cleanup_tasks::NewCleanupTask> = streams
        .iter()
        .map(|s| org_cleanup_tasks::NewCleanupTask {
            org_id: org_id.to_string(),
            org_name: org_name.to_string(),
            step: format!("delete_stream:{}/{}", s.stream_type, s.stream_name),
            step_order: ORDER_DELETE_STREAM_ITEM,
        })
        .collect();

    if !sub_tasks.is_empty() {
        org_cleanup_tasks::add_batch(&sub_tasks).await?;

        // Verify all sub-tasks were inserted
        let all = org_cleanup_tasks::list_by_org_status(org_id, None).await?;
        let inserted = all
            .iter()
            .filter(|t| t.step.starts_with("delete_stream:"))
            .count();
        if inserted != sub_tasks.len() {
            return Err(anyhow::anyhow!(
                "sub-task count mismatch: expected {} got {}",
                sub_tasks.len(),
                inserted
            ));
        }
    }

    Ok(())
}

async fn step_delete_stream(org_id: &str, type_and_name: &str) -> Result<(), anyhow::Error> {
    use config::meta::stream::StreamType;

    let (stream_type_str, stream_name) = type_and_name
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid stream key: {type_and_name}"))?;

    let stream_type = StreamType::from(stream_type_str);

    // Delete storage files in a loop to handle large streams
    loop {
        let files =
            infra::file_list::query_for_dump(org_id, stream_type, stream_name, (0, i64::MAX))
                .await?;
        if files.is_empty() {
            break;
        }
        for chunk in files.chunks(1000) {
            let paths: Vec<(&str, &str)> = chunk
                .iter()
                .map(|f| (f.account.as_str(), f.file.as_str()))
                .collect();
            infra::storage::del(paths).await?;
            for f in chunk {
                infra::file_list::remove(&f.file).await?;
            }
        }
    }

    // Delete the schema entry
    crate::service::db::schema::delete(org_id, stream_name, Some(stream_type)).await?;

    Ok(())
}

async fn step_delete_file_list(org_id: &str) -> Result<(), anyhow::Error> {
    infra::file_list::delete_by_org(org_id).await?;
    Ok(())
}

async fn step_delete_db_resources(org_id: &str) -> Result<(), anyhow::Error> {
    use infra::table::{
        action_scripts, alert_incidents, alerts, backfill_jobs, cipher, compactor_manual_jobs,
        dashboards, destinations, distinct_values, enrichment_table_urls, enrichment_tables,
        folders, incident_events, kv_store, org_storage_providers, re_pattern,
        re_pattern_stream_map, reports, search_queue, service_streams, short_urls, system_settings,
        templates, timed_annotations, trial_quota_usage,
    };

    // FK-constrained children must be deleted before their parents
    if let Err(e) = alert_incidents::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org alert_incidents error: {e}");
    }
    if let Err(e) = incident_events::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org incident_events error: {e}");
    }
    if let Err(e) = alerts::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org alerts error: {e}");
    }
    if let Err(e) = folders::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org folders error: {e}");
    }
    // timed_annotation_panels cascade from timed_annotations; both are deleted here
    // via the three-hop join: folders.org → dashboards.folder_id → timed_annotations.dashboard_id
    // Must run BEFORE dashboards::delete_by_org or the join finds no rows.
    if let Err(e) = timed_annotations::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org timed_annotations error: {e}");
    }
    if let Err(e) = dashboards::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org dashboards error: {e}");
    }
    if let Err(e) = templates::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org templates error: {e}");
    }
    if let Err(e) = destinations::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org destinations error: {e}");
    }
    if let Err(e) = reports::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org reports error: {e}");
    }
    if let Err(e) = action_scripts::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org action_scripts error: {e}");
    }
    if let Err(e) = kv_store::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org kv_store error: {e}");
    }
    if let Err(e) = cipher::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org cipher error: {e}");
    }
    if let Err(e) = enrichment_tables::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org enrichment_tables error: {e}");
    }
    if let Err(e) = enrichment_table_urls::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org enrichment_table_urls error: {e}");
    }
    if let Err(e) = backfill_jobs::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org backfill_jobs error: {e}");
    }
    if let Err(e) = search_queue::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org search_queue error: {e}");
    }
    if let Err(e) = re_pattern::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org re_pattern error: {e}");
    }
    if let Err(e) = re_pattern_stream_map::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org re_pattern_stream_map error: {e}");
    }
    if let Err(e) = org_storage_providers::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org org_storage_providers error: {e}");
    }
    if let Err(e) = trial_quota_usage::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org trial_quota_usage error: {e}");
    }
    if let Err(e) = service_streams::delete_all(org_id).await {
        log::error!("[org_cleanup] service_streams::delete_all error: {e}");
    }
    if let Err(e) = system_settings::delete_org_settings(org_id).await {
        log::error!("[org_cleanup] system_settings::delete_org_settings error: {e}");
    }

    // Delete pipelines (iterate-delete because there is no delete_by_org batch call)
    let pipelines = infra::pipeline::list_by_org(org_id).await?;
    for p in pipelines {
        infra::pipeline::delete(&p.id).await?;
    }

    // Delete saved views from the meta key-value store
    let db = infra::db::get_db().await;
    let prefix = format!("/organization/savedviews/{org_id}/");
    db.delete(&prefix, true, true, None)
        .await
        .map_err(|e| anyhow::anyhow!("saved_views delete error: {e}"))?;

    if let Err(e) = distinct_values::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org distinct_values error: {e}");
    }
    if let Err(e) = short_urls::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org short_urls error: {e}");
    }
    if let Err(e) = compactor_manual_jobs::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org compactor_manual_jobs error: {e}");
    }
    if let Err(e) = infra::table::search_job::search_jobs::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org search_jobs error: {e}");
    }

    Ok(())
}

async fn step_delete_scheduler_triggers(org_id: &str) -> Result<(), anyhow::Error> {
    let triggers = infra::scheduler::list_by_org(org_id, None).await?;
    for t in triggers {
        infra::scheduler::delete(&t.org, t.module, &t.module_key)
            .await
            .map_err(|e| anyhow::anyhow!("scheduler delete failed: {e}"))?;
    }
    Ok(())
}

#[allow(unused_variables)]
async fn step_delete_k8s_resources(org_id: &str) -> Result<(), anyhow::Error> {
    #[cfg(feature = "enterprise")]
    {
        log::info!("[org_cleanup] k8s resource cleanup for org={org_id} (enterprise)");
    }
    Ok(())
}

async fn step_delete_users(org_id: &str) -> Result<(), anyhow::Error> {
    let members = infra::table::org_users::list_users_by_org(org_id).await?;
    for member in &members {
        let user_orgs = infra::table::org_users::list_orgs_by_user(&member.email).await?;
        if user_orgs.len() <= 1 {
            // Only member of this org — delete the user entirely
            if let Err(e) = infra::table::users::remove(&member.email).await {
                log::error!("[org_cleanup] remove user {} error: {e}", member.email);
            }
            #[cfg(feature = "enterprise")]
            if let Err(e) = o2_openfga::authorizer::authz::delete_user_tuples(&member.email).await {
                return Err(anyhow::anyhow!(
                    "delete_user_tuples failed for {}: {e}",
                    member.email
                ));
            }
        }
        if let Err(e) = infra::table::org_users::remove(org_id, &member.email).await {
            log::error!(
                "[org_cleanup] remove org_user org={org_id} user={} error: {e}",
                member.email
            );
        }
    }
    Ok(())
}

async fn step_delete_ofga(org_id: &str) -> Result<(), anyhow::Error> {
    #[cfg(feature = "enterprise")]
    {
        o2_openfga::authorizer::authz::delete_org_tuples(org_id)
            .await
            .map_err(|e| anyhow::anyhow!("delete_org_tuples failed: {e}"))?;
    }
    #[cfg(not(feature = "enterprise"))]
    let _ = org_id;
    Ok(())
}

async fn step_delete_cloud_billing(org_id: &str) -> Result<(), anyhow::Error> {
    #[cfg(feature = "cloud")]
    {
        use o2_enterprise::enterprise::cloud::{
            billing_group, billing_invites, billings::cancel_org_subscription, customer_billings,
            org_invites,
        };

        // 1. Cancel Stripe subscription
        cancel_org_subscription(org_id).await?;

        // 2. Delete customer billing records
        customer_billings::delete_by_org_id(org_id).await?;

        // 3. Remove billing group memberships (payer and member sides)
        billing_group::delete_org_billing_group_memberships(org_id).await?;

        // 4. Delete billing group invites (sent and received)
        billing_invites::delete_org_billing_group_invites(org_id).await?;

        // 5. Delete pending org user invites
        org_invites::delete_all_org_invites(org_id).await?;
    }
    #[cfg(not(feature = "cloud"))]
    let _ = org_id;
    Ok(())
}

pub async fn initiate_deletion(org_id: &str, _initiated_by: &str) -> Result<(), anyhow::Error> {
    use crate::service::db::org_status;

    // Look up org — also gives us org_name for the cleanup tasks.
    let org = infra::table::organizations::get(org_id)
        .await
        .map_err(|e| anyhow::anyhow!("org not found: {e}"))?;

    // Atomic CAS: flip status active→deleting in the DB.
    // This is the single source-of-truth guard against concurrent requests on any cluster node.
    // The in-memory cache check below is a fast-path optimisation only.
    let won_race = infra::table::organizations::set_status_if(org_id, "active", "deleting")
        .await
        .map_err(|e| anyhow::anyhow!("failed to set org status: {e}"))?;

    if !won_race {
        return Err(anyhow::anyhow!("Organization is already being deleted"));
    }

    // Insert all fixed cleanup tasks (idempotent — on_conflict do_nothing).
    let tasks = fixed_steps(org_id, &org.org_name);
    org_cleanup_tasks::add_batch(&tasks).await?;

    // Broadcast to all cluster nodes so their in-memory caches reflect the new status.
    org_status::broadcast_deleting(org_id).await?;

    log::info!("[org_cleanup] initiated deletion for org={org_id}");
    Ok(())
}

async fn step_delete_org_record(org_id: &str) -> Result<(), anyhow::Error> {
    infra::table::organizations::remove(org_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::service::db::org_status::evict(org_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_steps_count() {
        let steps = fixed_steps("myorg", "My Org");
        assert_eq!(steps.len(), 9);
    }

    #[test]
    fn test_fixed_steps_order_ascending() {
        let steps = fixed_steps("myorg", "My Org");
        let orders: Vec<i32> = steps.iter().map(|s| s.step_order).collect();
        let mut sorted = orders.clone();
        sorted.sort();
        assert_eq!(orders, sorted, "step_orders must be strictly ascending");
    }

    #[test]
    fn test_fixed_steps_no_duplicates() {
        let steps = fixed_steps("myorg", "My Org");
        let mut seen = std::collections::HashSet::new();
        for s in &steps {
            assert!(seen.insert(s.step.clone()), "duplicate step: {}", s.step);
        }
    }

    #[test]
    fn test_fixed_steps_org_fields() {
        let steps = fixed_steps("acme", "Acme Corp");
        for s in &steps {
            assert_eq!(s.org_id, "acme");
            assert_eq!(s.org_name, "Acme Corp");
        }
    }

    #[test]
    fn test_fixed_steps_contains_all_expected() {
        let steps = fixed_steps("org", "Org");
        let names: Vec<&str> = steps.iter().map(|s| s.step.as_str()).collect();
        for expected in &[
            "delete_streams",
            "delete_file_list",
            "delete_db_resources",
            "delete_scheduler_triggers",
            "delete_k8s_resources",
            "delete_users",
            "delete_ofga",
            "delete_cloud_billing",
            "delete_org_record",
        ] {
            assert!(names.contains(expected), "missing step: {expected}");
        }
    }

    #[test]
    fn test_step_order_constants() {
        assert!(ORDER_DELETE_STREAMS < ORDER_DELETE_STREAM_ITEM);
        assert!(ORDER_DELETE_STREAM_ITEM < ORDER_DELETE_FILE_LIST);
        assert!(ORDER_DELETE_FILE_LIST < ORDER_DELETE_DB_RESOURCES);
        assert!(ORDER_DELETE_DB_RESOURCES < ORDER_DELETE_SCHEDULER_TRIGGERS);
        assert!(ORDER_DELETE_SCHEDULER_TRIGGERS < ORDER_DELETE_K8S_RESOURCES);
        assert!(ORDER_DELETE_K8S_RESOURCES < ORDER_DELETE_USERS);
        assert!(ORDER_DELETE_USERS < ORDER_DELETE_OFGA);
        assert!(ORDER_DELETE_OFGA < ORDER_DELETE_CLOUD_BILLING);
        assert!(ORDER_DELETE_CLOUD_BILLING < ORDER_DELETE_ORG_RECORD);
    }
}
