use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum NavigateSidebarSection {
    #[default]
    Spaces,
    Agents,
}

/// A client-only preview. Snapshot identity prevents Enter from using a reused workspace ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceNavigationTarget {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    boot_id: String,
    generation: Option<u64>,
}

/// A client-only agent preview. Snapshot identity prevents Enter from using a reused pane ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentNavigationTarget {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) pane_id: String,
    boot_id: String,
    generation: Option<u64>,
}

/// Display-only continuity while a direct focus request awaits its authoritative snapshot.
pub(super) struct PendingWorkspaceHighlight {
    pub(super) target: WorkspaceNavigationTarget,
    pub(super) request_id: String,
    expires_at: std::time::Instant,
}

impl WorkspaceNavigationTarget {
    pub(super) fn matches(&self, endpoint_id: &ClientEndpointId, workspace_id: &str) -> bool {
        &self.endpoint_id == endpoint_id && self.workspace_id == workspace_id
    }
}

impl AgentNavigationTarget {
    pub(super) fn matches(&self, endpoint_id: &ClientEndpointId, pane_id: &str) -> bool {
        &self.endpoint_id == endpoint_id && self.pane_id == pane_id
    }
}

impl ClientShellState {
    pub(crate) fn tick_workspace_highlight(&mut self, now: std::time::Instant) -> bool {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| now >= pending.expires_at)
        {
            self.pending_workspace_highlight = None;
            return true;
        }
        false
    }

    pub(super) fn reconcile_pending_workspace_highlight(&mut self) {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| {
                pending.target.endpoint_id != self.active_endpoint_id
                    || !self.navigation_target_valid(&pending.target)
                    || self.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.focused_workspace_id.as_deref()
                            == Some(pending.target.workspace_id.as_str())
                    })
            })
        {
            self.pending_workspace_highlight = None;
        }
    }

    pub(super) fn navigation_target(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
    ) -> Option<WorkspaceNavigationTarget> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot.as_deref()?;
        Some(WorkspaceNavigationTarget {
            endpoint_id: endpoint_id.clone(),
            workspace_id: workspace_id.to_owned(),
            boot_id: snapshot.boot_id.clone(),
            generation: endpoint.snapshot_generation,
        })
    }

    pub(super) fn focused_navigation_target(&self) -> Option<WorkspaceNavigationTarget> {
        let workspace_id = self.snapshot.as_deref()?.focused_workspace_id.as_deref()?;
        self.navigation_target(&self.active_endpoint_id, workspace_id)
    }

    pub(super) fn navigation_target_valid(&self, target: &WorkspaceNavigationTarget) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.endpoint_id == target.endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot_generation == target.generation
                && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                    snapshot.boot_id == target.boot_id
                        && snapshot
                            .workspaces
                            .iter()
                            .any(|workspace| workspace.workspace_id == target.workspace_id)
                })
        })
    }

    pub(super) fn workspace_preview_action_blocked(&self) -> bool {
        self.navigate_workspace_id.as_ref().is_some_and(|target| {
            target.endpoint_id != self.active_endpoint_id || !self.navigation_target_valid(target)
        })
    }

    pub(super) fn move_navigate_workspace(&mut self, delta: isize) {
        let mobile = self.mobile_layout_active();
        let surface_available = self.snapshot.is_some() && self.pane_surface.is_some();
        let empty_collapsed_groups = HashSet::new();
        let mut targets = Vec::new();
        for endpoint in &self.endpoints {
            if endpoint.status != ClientEndpointStatus::Online {
                continue;
            }
            let Some(snapshot) = endpoint.snapshot.as_deref() else {
                continue;
            };
            let entries = if self.sidebar_collapsed && !mobile && surface_available {
                snapshot
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(index, _)| WorkspaceEntry {
                        index,
                        indented: false,
                        last_child: false,
                    })
                    .collect()
            } else {
                let collapsed_groups = if mobile && surface_available {
                    &empty_collapsed_groups
                } else {
                    self.collapsed_groups_for_endpoint(&endpoint.endpoint_id)
                        .unwrap_or(&empty_collapsed_groups)
                };
                render::workspace_entries(snapshot, collapsed_groups)
            };
            for entry in entries {
                targets.push(WorkspaceNavigationTarget {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    workspace_id: snapshot.workspaces[entry.index].workspace_id.clone(),
                    boot_id: snapshot.boot_id.clone(),
                    generation: endpoint.snapshot_generation,
                });
            }
        }
        if targets.is_empty() {
            return;
        }
        let current = self
            .navigate_workspace_id
            .as_ref()
            .and_then(|selected| targets.iter().position(|target| target == selected));
        let next = match current {
            Some(current) if mobile => {
                (current as isize + delta).clamp(0, targets.len() as isize - 1) as usize
            }
            Some(current) => (current as isize + delta).rem_euclid(targets.len() as isize) as usize,
            None if delta < 0 => targets.len() - 1,
            None => 0,
        };
        let target = targets.swap_remove(next);
        self.collapsed_endpoints.remove(&target.endpoint_id);
        if self.endpoints.len() == 1 && !mobile {
            self.reveal_workspace(&target.workspace_id);
        }
        self.navigate_workspace_id = Some(target);
        self.reveal_mobile_workspace = mobile;
        self.reveal_navigation_workspace =
            !mobile || self.snapshot.is_none() || self.pane_surface.is_none();
    }

    pub(super) fn accept_navigate_workspace(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.navigate_workspace_id.clone() else {
            self.leave_navigate_mode();
            outcome.repaint = true;
            return;
        };
        if !self.navigation_target_valid(&target) {
            self.receive_endpoint_unavailable(
                "Workspace is no longer available; select a connected workspace".into(),
            );
            outcome.repaint = true;
            return;
        }
        let action_index = outcome.actions.len();
        if self.focus_or_activate(
            target.endpoint_id.clone(),
            ClientEndpointFocusTarget::Workspace(target.workspace_id.clone()),
            outcome,
        ) {
            // Endpoint handoffs already retain their coherent source frame in the runtime.
            // Only retain a highlight if a focus request was actually enqueued.
            if let Some(ClientShellAction::Endpoint { request, .. }) =
                outcome.actions.get(action_index)
            {
                self.pending_workspace_highlight = Some(PendingWorkspaceHighlight {
                    target,
                    request_id: request.id.clone(),
                    // A later focus can be coalesced with this one before a snapshot is sent.
                    expires_at: std::time::Instant::now() + std::time::Duration::from_secs(1),
                });
                self.reconcile_pending_workspace_highlight();
            }
            self.leave_navigate_mode();
        }
        outcome.repaint = true;
    }

    pub(super) fn agent_navigation_target(
        &self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
    ) -> Option<AgentNavigationTarget> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot.as_deref()?;
        snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id == pane_id)
            .then(|| AgentNavigationTarget {
                endpoint_id: endpoint_id.clone(),
                pane_id: pane_id.to_owned(),
                boot_id: snapshot.boot_id.clone(),
                generation: endpoint.snapshot_generation,
            })
    }

    pub(super) fn agent_navigation_target_valid(&self, target: &AgentNavigationTarget) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.endpoint_id == target.endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot_generation == target.generation
                && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                    snapshot.boot_id == target.boot_id
                        && snapshot
                            .agents
                            .iter()
                            .any(|agent| agent.pane_id == target.pane_id)
                })
        })
    }

    /// Agent list order for Navigate-mode Agents focus. Matches the sidebar
    /// render path: single-endpoint uses `ordered_agent_pane_ids`, multi-endpoint
    /// uses aggregate `online_agent_targets`.
    pub(super) fn navigate_visible_agent_targets(
        &self,
    ) -> Vec<super::aggregate_navigation::AggregateAgentTarget> {
        if self.endpoints.len() == 1 {
            let Some(snapshot) = self.snapshot.as_deref() else {
                return Vec::new();
            };
            return super::agent_sidebar::ordered_agent_pane_ids(
                snapshot,
                self.config.agent_panel_sort,
            )
            .into_iter()
            .map(
                |pane_id| super::aggregate_navigation::AggregateAgentTarget {
                    endpoint_id: self.active_endpoint_id.clone(),
                    pane_id,
                },
            )
            .collect();
        }
        super::aggregate_navigation::online_agent_targets(
            &self.endpoints,
            &self.active_endpoint_id,
            self.config.agent_panel_sort,
        )
    }

    pub(super) fn seed_navigate_agent_from_focus(&mut self) {
        let agents = self.navigate_visible_agent_targets();
        if agents.is_empty() {
            self.navigate_agent = None;
            return;
        }
        let focused = self.snapshot.as_deref().and_then(|snapshot| {
            let pane_id = snapshot.focused_pane_id.as_deref()?;
            agents
                .iter()
                .find(|target| {
                    target.endpoint_id == self.active_endpoint_id && target.pane_id == pane_id
                })
                .or_else(|| agents.first())
        });
        let Some(selected) = focused.or_else(|| agents.first()) else {
            self.navigate_agent = None;
            return;
        };
        let Some(target) = self.agent_navigation_target(&selected.endpoint_id, &selected.pane_id)
        else {
            self.navigate_agent = None;
            return;
        };
        self.reveal_endpoint_agent(
            &target.endpoint_id,
            &target.pane_id,
            self.hits.agent_body.height,
        );
        self.navigate_agent = Some(target);
    }

    pub(super) fn set_navigate_section(&mut self, section: NavigateSidebarSection) {
        self.navigate_section = section;
        match section {
            NavigateSidebarSection::Spaces => {
                if self.navigate_workspace_id.is_none() {
                    self.navigate_workspace_id = self.focused_navigation_target();
                }
                if let Some(target) = self.navigate_workspace_id.clone() {
                    self.collapsed_endpoints.remove(&target.endpoint_id);
                    if self.endpoints.len() == 1 && !self.mobile_layout_active() {
                        self.reveal_workspace(&target.workspace_id);
                    }
                    self.reveal_navigation_workspace = true;
                }
            }
            NavigateSidebarSection::Agents => {
                if self
                    .navigate_agent
                    .as_ref()
                    .is_none_or(|target| !self.agent_navigation_target_valid(target))
                {
                    self.seed_navigate_agent_from_focus();
                } else if let Some(target) = self.navigate_agent.clone() {
                    self.reveal_endpoint_agent(
                        &target.endpoint_id,
                        &target.pane_id,
                        self.hits.agent_body.height,
                    );
                }
            }
        }
    }

    pub(super) fn move_navigate_agent(&mut self, delta: isize) {
        let agents = self.navigate_visible_agent_targets();
        if agents.is_empty() {
            self.navigate_agent = None;
            return;
        }
        let current = self.navigate_agent.as_ref().and_then(|selected| {
            agents.iter().position(|target| {
                target.endpoint_id == selected.endpoint_id && target.pane_id == selected.pane_id
            })
        });
        let next = match current {
            Some(current) => (current as isize + delta).rem_euclid(agents.len() as isize) as usize,
            None if delta < 0 => agents.len() - 1,
            None => 0,
        };
        let selected = &agents[next];
        let Some(target) = self.agent_navigation_target(&selected.endpoint_id, &selected.pane_id)
        else {
            return;
        };
        self.reveal_endpoint_agent(
            &target.endpoint_id,
            &target.pane_id,
            self.hits.agent_body.height,
        );
        self.navigate_agent = Some(target);
    }

    pub(super) fn focus_navigate_agent_at(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let agents = self.navigate_visible_agent_targets();
        let Some(selected) = agents.get(index) else {
            return false;
        };
        let Some(target) = self.agent_navigation_target(&selected.endpoint_id, &selected.pane_id)
        else {
            return false;
        };
        self.navigate_agent = Some(target);
        self.accept_navigate_agent(outcome);
        true
    }

    pub(super) fn accept_navigate_agent(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.navigate_agent.clone() else {
            self.leave_navigate_mode();
            outcome.repaint = true;
            return;
        };
        if !self.agent_navigation_target_valid(&target) {
            self.receive_endpoint_unavailable(
                "Agent is no longer available; select a connected agent".into(),
            );
            outcome.repaint = true;
            return;
        }
        if self.focus_or_activate(
            target.endpoint_id.clone(),
            ClientEndpointFocusTarget::Pane(target.pane_id.clone()),
            outcome,
        ) {
            self.leave_navigate_mode();
        }
        outcome.repaint = true;
    }

    pub(super) fn clear_navigate_selection(&mut self) {
        self.navigate_workspace_id = None;
        self.navigate_agent = None;
        self.navigate_section = NavigateSidebarSection::Spaces;
    }

    pub(super) fn leave_navigate_mode(&mut self) {
        self.mode = self.copy_or_terminal_mode();
        self.clear_navigate_selection();
    }
}
