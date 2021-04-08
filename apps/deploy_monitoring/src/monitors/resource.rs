// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use getset::Getters;
use merge::Merge;
use serde::Serialize;
use slog::{error, Logger};
use sysinfo::{System, SystemExt};

use shell::stats::memory::ProcessMemoryStatsMaxMerge;

use crate::constants::{MEASUREMENTS_MAX_CAPACITY, OCAML_PORT, TEZEDGE_PORT};
use crate::display_info::DiskData;
use crate::monitors::Alerts;
use crate::node::OcamlNode;
use crate::node::{Node, TezedgeNode};
use crate::slack::SlackServer;

pub type ResourceUtilizationStorage = Arc<RwLock<VecDeque<ResourceUtilization>>>;
pub type ResourceUtilizationStorageMap = HashMap<&'static str, ResourceUtilizationStorage>;

pub struct ResourceMonitor {
    resource_utilization: ResourceUtilizationStorageMap,
    last_checked_head_level: Option<u64>,
    alerts: Alerts,
    log: Logger,
    slack: Option<SlackServer>,
    system: System,
}

#[derive(Clone, Debug, Serialize, Getters, Merge, Default)]
pub struct MemoryStats {
    #[get = "pub(crate)"]
    #[merge(strategy = merge::ord::max)]
    node: ProcessMemoryStatsMaxMerge,

    // TODO: TE-499 remove protocol_runners and use validators for ocaml and tezedge type
    #[get = "pub(crate)"]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[merge(strategy = merge::ord::max)]
    protocol_runners: Option<ProcessMemoryStatsMaxMerge>,

    #[get = "pub(crate)"]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[merge(strategy = merge::ord::max)]
    validators: Option<ProcessMemoryStatsMaxMerge>,
}

#[derive(Clone, Debug, Serialize, Getters, Merge)]
pub struct ResourceUtilization {
    #[get = "pub(crate)"]
    #[merge(strategy = merge::ord::max)]
    timestamp: i64,

    #[get = "pub(crate)"]
    memory: MemoryStats,

    #[get = "pub(crate)"]
    #[merge(strategy = merge::ord::max)]
    disk: DiskData,

    #[get = "pub(crate)"]
    cpu: CpuStats,
}

#[derive(Clone, Debug, Serialize, Getters, Merge, Default)]
pub struct CpuStats {
    #[get = "pub(crate)"]
    #[merge(strategy = merge::ord::max)]
    node: i32,

    #[get = "pub(crate)"]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[merge(strategy = merge::ord::max)]
    protocol_runners: Option<i32>,
}

impl ResourceMonitor {
    pub fn new(
        resource_utilization: ResourceUtilizationStorageMap,
        last_checked_head_level: Option<u64>,
        alerts: Alerts,
        log: Logger,
        slack: Option<SlackServer>,
    ) -> Self {
        Self {
            resource_utilization,
            last_checked_head_level,
            alerts,
            log,
            slack,
            system: System::new_all(),
        }
    }

    pub async fn take_measurement(&mut self) -> Result<(), failure::Error> {
        let ResourceMonitor {
            system,
            resource_utilization,
            log,
            last_checked_head_level,
            alerts,
            slack,
            ..
        } = self;

        system.refresh_all();

        for (node_tag, resource_storage) in resource_utilization {
            let node_resource_measurement = if node_tag == &"tezedge" {
                let tezedge_node = TezedgeNode::collect_memory_data(TEZEDGE_PORT).await?;
                let protocol_runners =
                    TezedgeNode::collect_protocol_runners_memory_stats(TEZEDGE_PORT).await?;
                let tezedge_disk = TezedgeNode::collect_disk_data()?;

                let tezedge_cpu = TezedgeNode::collect_cpu_data(system, "light-node")?;
                let protocol_runners_cpu =
                    TezedgeNode::collect_cpu_data(system, "protocol-runner")?;
                let resources = ResourceUtilization {
                    timestamp: chrono::Local::now().timestamp(),
                    memory: MemoryStats {
                        node: tezedge_node,
                        protocol_runners: Some(protocol_runners),
                        validators: None,
                    },
                    disk: tezedge_disk,
                    cpu: CpuStats {
                        node: tezedge_cpu,
                        protocol_runners: Some(protocol_runners_cpu),
                    },
                };
                let current_head_level =
                    *TezedgeNode::collect_head_data(TEZEDGE_PORT).await?.level();
                handle_alerts(
                    node_tag,
                    resources.clone(),
                    current_head_level,
                    last_checked_head_level,
                    slack.clone(),
                    alerts,
                    log,
                )
                .await?;
                resources
            } else {
                let ocaml_node = OcamlNode::collect_memory_data(OCAML_PORT).await?;
                let tezos_validators = OcamlNode::collect_validator_memory_stats()?;
                let ocaml_disk = OcamlNode::collect_disk_data()?;
                let ocaml_cpu = OcamlNode::collect_cpu_data(system, "tezos-node")?;

                let resources = ResourceUtilization {
                    timestamp: chrono::Local::now().timestamp(),
                    memory: MemoryStats {
                        node: ocaml_node,
                        protocol_runners: None,
                        validators: Some(tezos_validators),
                    },
                    disk: ocaml_disk,
                    cpu: CpuStats {
                        node: ocaml_cpu,
                        protocol_runners: None,
                    },
                };
                let current_head_level = *OcamlNode::collect_head_data(OCAML_PORT).await?.level();
                handle_alerts(
                    node_tag,
                    resources.clone(),
                    current_head_level,
                    last_checked_head_level,
                    slack.clone(),
                    alerts,
                    log,
                )
                .await?;
                resources
            };

            match &mut resource_storage.write() {
                Ok(resources_locked) => {
                    if resources_locked.len() == MEASUREMENTS_MAX_CAPACITY {
                        resources_locked.pop_back();
                    }

                    resources_locked.push_front(node_resource_measurement.clone());
                }
                Err(e) => error!(log, "Resource lock poisoned, reason => {}", e),
            }
        }
        Ok(())
    }
}

async fn handle_alerts(
    node_tag: &str,
    last_measurement: ResourceUtilization,
    current_head_level: u64,
    last_checked_head_level: &mut Option<u64>,
    slack: Option<SlackServer>,
    alerts: &mut Alerts,
    log: &Logger,
) -> Result<(), failure::Error> {
    // current time timestamp
    let current_time = Utc::now().timestamp();

    // let current_head_level = *TezedgeNode::collect_head_data(TEZEDGE_PORT).await?.level();

    alerts
        .check_disk_alert(node_tag, slack.as_ref(), current_time)
        .await?;
    alerts
        .check_memory_alert(
            node_tag,
            slack.as_ref(),
            current_time,
            last_measurement.clone(),
        )
        .await?;
    alerts
        .check_node_stuck_alert(
            node_tag,
            last_checked_head_level,
            current_head_level,
            current_time,
            slack.as_ref(),
            log,
        )
        .await?;

    alerts
        .check_cpu_alert(
            node_tag,
            slack.as_ref(),
            current_time,
            last_measurement.clone(),
        )
        .await?;
    *last_checked_head_level = Some(current_head_level);
    Ok(())
}
