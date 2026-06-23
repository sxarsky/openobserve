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

#[allow(unused_variables)]
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
        .enumerate()
        .map(|(i, s)| org_cleanup_tasks::NewCleanupTask {
            org_id: org_id.to_string(),
            org_name: org_name.to_string(),
            step: format!("delete_stream:{}/{}", s.stream_type, s.stream_name),
            // Use step order just below ORDER_DELETE_FILE_LIST so they run before it
            step_order: ORDER_DELETE_STREAMS + 1 + i as i32,
        })
        .collect();

    if !sub_tasks.is_empty() {
        org_cleanup_tasks::add_batch(&sub_tasks).await?;
    }

    Ok(())
}

async fn step_delete_stream(org_id: &str, type_and_name: &str) -> Result<(), anyhow::Error> {
    use config::meta::stream::StreamType;

    let (stream_type_str, stream_name) = type_and_name
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid stream key: {type_and_name}"))?;

    let stream_type = StreamType::from(stream_type_str);

    // Delete storage files
    let files =
        infra::file_list::query_for_dump(org_id, stream_type, stream_name, (0, i64::MAX)).await?;

    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|f| (f.account.as_str(), f.file.as_str()))
        .collect();

    if !file_refs.is_empty() {
        if let Err(e) = infra::storage::del(file_refs).await {
            log::error!("[org_cleanup] delete storage files for stream {type_and_name} error: {e}");
        }
    }

    for f in &files {
        if let Err(e) = infra::file_list::remove(&f.file).await {
            log::error!("[org_cleanup] remove file_list entry {} error: {e}", f.file);
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
        action_scripts, alert_incidents, alerts, backfill_jobs, cipher, dashboards, destinations,
        enrichment_table_urls, enrichment_tables, folders, incident_events, kv_store,
        org_storage_providers, re_pattern, re_pattern_stream_map, reports, search_queue,
        service_streams, system_settings, templates, trial_quota_usage,
    };

    if let Err(e) = alerts::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org alerts error: {e}");
    }
    if let Err(e) = folders::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org folders error: {e}");
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
    if let Err(e) = alert_incidents::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org alert_incidents error: {e}");
    }
    if let Err(e) = incident_events::delete_by_org(org_id).await {
        log::error!("[org_cleanup] delete_by_org incident_events error: {e}");
    }
    if let Err(e) = service_streams::delete_all(org_id).await {
        log::error!("[org_cleanup] service_streams::delete_all error: {e}");
    }
    if let Err(e) = system_settings::delete_org_settings(org_id).await {
        log::error!("[org_cleanup] system_settings::delete_org_settings error: {e}");
    }

    // TODO: add delete_by_org for timed_annotations (no org_id column, linked via dashboard_id)
    // TODO: add delete_by_org for timed_annotation_panels (no org_id column)
    // TODO: add delete_by_org for distinct_values (sqlx-based, different structure)
    // TODO: add delete_by_org for short_urls (no org column)
    // TODO: add delete_by_org for compactor_manual_jobs (no org column confirmed)
    // TODO: add delete_by_org for search_job (no direct org column in entity)

    Ok(())
}

async fn step_delete_scheduler_triggers(org_id: &str) -> Result<(), anyhow::Error> {
    let triggers = infra::scheduler::list_by_org(org_id, None).await?;
    for t in triggers {
        let org = t.org.clone();
        let module_key = t.module_key.clone();
        let module_str = format!("{:?}", t.module);
        if let Err(e) = infra::scheduler::delete(&org, t.module, &module_key).await {
            log::error!(
                "[org_cleanup] delete trigger org={org} module={module_str} key={module_key} error: {e}"
            );
        }
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

#[allow(unused_variables)]
async fn step_delete_ofga(org_id: &str) -> Result<(), anyhow::Error> {
    #[cfg(feature = "enterprise")]
    {
        log::info!("[org_cleanup] OpenFGA cleanup for org={org_id} (enterprise)");
    }
    Ok(())
}

#[allow(unused_variables)]
async fn step_delete_cloud_billing(org_id: &str) -> Result<(), anyhow::Error> {
    #[cfg(feature = "cloud")]
    {
        log::info!("[org_cleanup] cloud billing cleanup for org={org_id} (cloud)");
    }
    Ok(())
}

async fn step_delete_org_record(org_id: &str) -> Result<(), anyhow::Error> {
    infra::table::organizations::remove(org_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::service::db::org_status::evict(org_id).await?;
    Ok(())
}
