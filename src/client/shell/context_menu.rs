use super::*;

impl ClientContextMenuOverlay {
    pub(super) fn items(&self) -> Vec<ClientContextMenuItem<'_>> {
        use ClientContextMenuAction as Action;

        let mut items = self.builtin_items();
        // Appended, so the default highlight is never a plugin action.
        items.extend(
            self.plugin_actions
                .iter()
                .enumerate()
                .map(|(index, action)| ClientContextMenuItem {
                    label: &action.title,
                    action: Action::Plugin(index),
                }),
        );
        items
    }

    fn builtin_items(&self) -> Vec<ClientContextMenuItem<'_>> {
        use ClientContextMenuAction as Action;

        let item = |label, action| ClientContextMenuItem { label, action };
        match &self.target {
            ClientContextMenuTarget::Workspace { is_git: false, .. } => {
                vec![item("Rename", Action::Rename), item("Close", Action::Close)]
            }
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: false,
                has_worktree_children: false,
                ..
            } => vec![
                item("Rename", Action::Rename),
                item("Close", Action::Close),
                item("New worktree", Action::NewWorktree),
                item("Open worktree...", Action::OpenWorktree),
            ],
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: true,
                ..
            } => vec![
                item("Rename", Action::Rename),
                item("Close", Action::Close),
                item("Delete worktree checkout...", Action::RemoveWorktree),
            ],
            ClientContextMenuTarget::Workspace {
                has_worktree_children: true,
                collapsed,
                ..
            } => vec![
                item("Rename", Action::Rename),
                item("Close group", Action::Close),
                item("New worktree", Action::NewWorktree),
                item("Open worktree...", Action::OpenWorktree),
                item(
                    if *collapsed { "Expand" } else { "Collapse" },
                    Action::ToggleGroup,
                ),
            ],
            ClientContextMenuTarget::Tab { .. } => vec![
                item("New tab", Action::NewTab),
                item("Rename", Action::Rename),
                item("Close", Action::Close),
            ],
            ClientContextMenuTarget::Pane {
                source_pane_id,
                has_manual_label,
                right_click_passthrough,
                ..
            } => {
                let mut items = vec![item("Rename pane", Action::RenamePane)];
                if *has_manual_label {
                    items.push(item("Clear pane name", Action::ClearPaneName));
                }
                if source_pane_id.is_some() {
                    items.push(item("Swap with focused pane", Action::SwapWithFocusedPane));
                }
                items.extend([
                    item("Split right", Action::SplitRight),
                    item("Split down", Action::SplitDown),
                    item("Zoom", Action::Zoom),
                    item("Send file...", Action::SendFile),
                    item("Receive file...", Action::ReceiveFile),
                    item(
                        if *right_click_passthrough {
                            "Use Herdr right-click menu"
                        } else {
                            "Send right-clicks to pane"
                        },
                        Action::ToggleRightClickPassthrough,
                    ),
                    item("Close pane", Action::ClosePane),
                ]);
                items
            }
        }
    }
}

impl ClientShellState {
    fn snapshot_plugin_actions(
        &self,
        context: crate::protocol::ClientShellPluginActionContext,
    ) -> Vec<crate::protocol::ClientShellPluginAction> {
        self.snapshot
            .as_deref()
            .map(|snapshot| {
                snapshot
                    .plugin_actions
                    .iter()
                    .filter(|action| action.contexts.contains(&context))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn open_workspace_context_menu(&mut self, workspace_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return;
        };
        let worktree = workspace.worktree.as_ref();
        let has_worktree_children = worktree.is_some_and(|worktree| {
            !worktree.is_linked_worktree
                && snapshot
                    .workspaces
                    .iter()
                    .filter(|candidate| {
                        candidate
                            .worktree
                            .as_ref()
                            .is_some_and(|candidate| candidate.key == worktree.key)
                    })
                    .count()
                    >= 2
        });
        let collapsed =
            worktree.is_some_and(|worktree| self.collapsed_groups.contains(&worktree.key));
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace {
                workspace_id,
                is_git: worktree.is_some() || workspace.branch.is_some(),
                is_linked_worktree: worktree.is_some_and(|worktree| worktree.is_linked_worktree),
                has_worktree_children,
                collapsed,
            },
            x,
            y,
            highlighted: 0,
            plugin_actions: self.snapshot_plugin_actions(
                crate::protocol::ClientShellPluginActionContext::Workspace,
            ),
        }));
    }

    pub(super) fn open_tab_context_menu(&mut self, tab_id: String, x: u16, y: u16) {
        let Some(tab) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id))
        else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Tab {
                tab_id,
                workspace_id: tab.workspace_id.clone(),
            },
            x,
            y,
            highlighted: 0,
            plugin_actions: Vec::new(),
        }));
    }

    pub(super) fn open_pane_context_menu(&mut self, pane_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let source_pane_id = snapshot
            .focused_pane_id
            .clone()
            .filter(|focused| focused != &pane_id);
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Pane {
                pane_id,
                workspace_id: pane.workspace_id.clone(),
                source_pane_id,
                has_manual_label: pane.label.is_some(),
                right_click_passthrough: pane.right_click_passthrough,
            },
            x,
            y,
            highlighted: 0,
            plugin_actions: self
                .snapshot_plugin_actions(crate::protocol::ClientShellPluginActionContext::Pane),
        }));
    }

    pub(super) fn move_context_menu_selection(&mut self, delta: isize) {
        // A menu taller than the screen draws only what fits, and only a drawn
        // entry has a hit rect. Plugin entries make that reachable, so keep the
        // highlight inside the same set the mouse can reach: an entry runs
        // without a confirmation step, and an undrawn one shows no highlight.
        let drawn = self.hits.context_menu_rows.len();
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let mut item_count = menu.items().len();
        if drawn > 0 {
            item_count = item_count.min(drawn);
        }
        if item_count == 0 {
            return;
        }
        menu.highlighted = (menu.highlighted as isize + delta)
            .clamp(0, item_count.saturating_sub(1) as isize) as usize;
    }

    pub(super) fn activate_context_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.take() else {
            return;
        };
        let Some(action) = menu.items().get(index).map(|item| item.action) else {
            outcome.repaint = true;
            return;
        };
        if let ClientContextMenuAction::Plugin(index) = action {
            self.invoke_context_menu_plugin_action(index, &menu, outcome);
            outcome.repaint = true;
            return;
        }
        match menu.target {
            ClientContextMenuTarget::Workspace { workspace_id, .. } => {
                self.activate_workspace_context_action(workspace_id, action, outcome)
            }
            ClientContextMenuTarget::Tab {
                tab_id,
                workspace_id,
            } => self.activate_tab_context_action(tab_id, workspace_id, action, outcome),
            ClientContextMenuTarget::Pane {
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                ..
            } => self.activate_pane_context_action(
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                action,
                outcome,
            ),
        }
        outcome.repaint = true;
    }

    fn invoke_context_menu_plugin_action(
        &mut self,
        index: usize,
        menu: &ClientContextMenuOverlay,
        outcome: &mut ClientShellInput,
    ) {
        let Some(action) = menu.plugin_actions.get(index) else {
            return;
        };
        let (workspace_id, pane_id) = match &menu.target {
            ClientContextMenuTarget::Workspace { workspace_id, .. }
            | ClientContextMenuTarget::Tab { workspace_id, .. } => (workspace_id.clone(), None),
            ClientContextMenuTarget::Pane {
                workspace_id,
                pane_id,
                ..
            } => (workspace_id.clone(), Some(pane_id.clone())),
        };
        self.push_endpoint_method(
            crate::api::schema::Method::PluginActionInvoke(
                crate::api::schema::PluginActionInvokeParams {
                    action_id: action.action_id.clone(),
                    plugin_id: None,
                    // Clicking an entry does not move Herdr's focus, so the
                    // clicked target has to scope the invocation itself or the
                    // endpoint fills the context from whatever is focused.
                    context: Some(crate::api::schema::PluginInvocationContext {
                        workspace_id: Some(workspace_id),
                        focused_pane_id: pane_id,
                        invocation_source: Some("context_menu".to_owned()),
                        ..Default::default()
                    }),
                },
            ),
            outcome,
        );
    }

    fn activate_workspace_context_action(
        &mut self,
        workspace_id: String,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::input::KeybindAction;

        match action {
            ClientContextMenuAction::Rename => {
                let label = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename workspace",
                        input: label,
                        replace_on_type: false,
                        target: ClientRenameTarget::Workspace { workspace_id },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                if self.config.confirm_close {
                    self.open_confirm_close_overlay(workspace_id);
                } else {
                    self.push_endpoint_method(
                        crate::api::schema::Method::WorkspaceClose(
                            crate::api::schema::WorkspaceCloseParams {
                                workspace_id,
                                close_group: true,
                            },
                        ),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::NewWorktree => {
                self.begin_worktree_action_for(KeybindAction::NewWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::OpenWorktree => {
                self.begin_worktree_action_for(KeybindAction::OpenWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::RemoveWorktree => {
                self.begin_worktree_action_for(KeybindAction::RemoveWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::ToggleGroup => {
                let key = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == workspace_id)
                        .and_then(|workspace| workspace.worktree.as_ref())
                        .map(|worktree| worktree.key.clone())
                });
                if let Some(key) = key {
                    if !self.collapsed_groups.remove(&key) {
                        self.collapsed_groups.insert(key);
                    }
                    self.persist_chrome_preferences(outcome);
                }
            }
            _ => {}
        }
    }

    fn activate_tab_context_action(
        &mut self,
        tab_id: String,
        workspace_id: String,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{Method, TabTarget};

        self.push_endpoint_method(
            Method::TabFocus(TabTarget {
                tab_id: tab_id.clone(),
            }),
            outcome,
        );
        match action {
            ClientContextMenuAction::NewTab => {
                if self.config.prompt_new_tab_name {
                    let default_name = (self
                        .snapshot
                        .as_deref()
                        .map(|snapshot| {
                            snapshot
                                .tabs
                                .iter()
                                .filter(|tab| tab.workspace_id == workspace_id)
                                .count()
                        })
                        .unwrap_or(0)
                        + 1)
                    .to_string();
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "new tab",
                        input: default_name.clone(),
                        replace_on_type: true,
                        target: ClientRenameTarget::NewTab {
                            workspace_id,
                            default_name,
                        },
                    }));
                } else {
                    self.push_endpoint_method(
                        Method::TabCreate(crate::api::schema::TabCreateParams {
                            workspace_id: Some(workspace_id),
                            cwd: None,
                            focus: true,
                            label: None,
                            env: Default::default(),
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::Rename => {
                let tab = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id));
                if let Some(tab) = tab {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename tab",
                        input: tab.label.clone(),
                        replace_on_type: false,
                        target: ClientRenameTarget::Tab {
                            tab_id,
                            auto_name: !tab.custom_label,
                            original_name: tab.label.clone(),
                        },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                self.push_endpoint_method(Method::TabClose(TabTarget { tab_id }), outcome);
            }
            _ => {}
        }
    }

    fn activate_pane_context_action(
        &mut self,
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        right_click_passthrough: bool,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{
            Method, PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection,
        };

        match action {
            ClientContextMenuAction::RenamePane => {
                let label = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: "rename pane",
                    input: label.clone().unwrap_or_default(),
                    replace_on_type: label.is_none(),
                    target: ClientRenameTarget::Pane { pane_id },
                }));
            }
            ClientContextMenuAction::ClearPaneName => self.push_endpoint_method(
                Method::PaneRename(PaneRenameParams {
                    pane_id,
                    label: None,
                }),
                outcome,
            ),
            ClientContextMenuAction::SwapWithFocusedPane => {
                if let Some(source_pane_id) = source_pane_id {
                    self.push_endpoint_method(
                        Method::PaneSwap(PaneSwapParams {
                            pane_id: None,
                            direction: None,
                            source_pane_id: Some(source_pane_id.clone()),
                            target_pane_id: Some(pane_id),
                        }),
                        outcome,
                    );
                    self.push_endpoint_method(
                        Method::PaneFocus(PaneTarget {
                            pane_id: source_pane_id,
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::SplitRight | ClientContextMenuAction::SplitDown => {
                self.push_endpoint_method(
                    Method::PaneSplit(PaneSplitParams {
                        workspace_id: Some(workspace_id),
                        target_pane_id: Some(pane_id),
                        direction: if action == ClientContextMenuAction::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                        ratio: None,
                        cwd: None,
                        focus: true,
                        right_click: Default::default(),
                        env: Default::default(),
                    }),
                    outcome,
                );
            }
            ClientContextMenuAction::Zoom => self.push_endpoint_method(
                Method::PaneZoom(PaneZoomParams {
                    pane_id: Some(pane_id),
                    mode: PaneZoomMode::Toggle,
                }),
                outcome,
            ),
            ClientContextMenuAction::ToggleRightClickPassthrough => self.push_endpoint_method(
                Method::PaneInputSet(PaneInputSetParams {
                    pane_id,
                    right_click: if right_click_passthrough {
                        PaneRightClickTarget::Herdr
                    } else {
                        PaneRightClickTarget::Pane
                    },
                }),
                outcome,
            ),
            ClientContextMenuAction::ClosePane => {
                self.push_endpoint_method(Method::PaneClose(PaneTarget { pane_id }), outcome)
            }
            // The transfer runs against the focused pane's working directory,
            // so focus the pane the menu was opened on before starting.
            ClientContextMenuAction::SendFile | ClientContextMenuAction::ReceiveFile => {
                self.push_endpoint_method(Method::PaneFocus(PaneTarget { pane_id }), outcome);
                if action == ClientContextMenuAction::SendFile {
                    self.open_file_transfer_send();
                } else {
                    self.open_file_transfer_receive(outcome);
                }
            }
            _ => {}
        }
    }
}
