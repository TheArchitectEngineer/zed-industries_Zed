use crate::{attach_modal::AttachModal, debugger_panel_item::DebugPanelItem};
use anyhow::Result;
use collections::{BTreeMap, HashMap};
use command_palette_hooks::CommandPaletteFilter;
use dap::{
    client::DebugAdapterClientId,
    debugger_settings::DebuggerSettings,
    messages::{Events, Message},
    requests::{Request, RunInTerminal, StartDebugging},
    Capabilities, CapabilitiesEvent, ContinuedEvent, ErrorResponse, ExitedEvent, LoadedSourceEvent,
    ModuleEvent, OutputEvent, RunInTerminalRequestArguments, RunInTerminalResponse, StoppedEvent,
    TerminatedEvent, ThreadEvent, ThreadEventReason,
};
use gpui::{
    actions, Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Subscription, Task, WeakEntity,
};
use project::{
    debugger::{
        dap_store::{DapStore, DapStoreEvent},
        session::ThreadId,
    },
    terminals::TerminalKind,
};
use rpc::proto::{self, UpdateDebugAdapter};
use serde_json::Value;
use settings::Settings;
use std::{any::TypeId, collections::VecDeque, path::PathBuf, u64};
use task::DebugRequestType;
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    pane, Continue, Disconnect, Pane, Pause, Restart, Start, StepBack, StepInto, StepOut, StepOver,
    Stop, ToggleIgnoreBreakpoints, Workspace,
};

pub enum DebugPanelEvent {
    Exited(DebugAdapterClientId),
    Terminated(DebugAdapterClientId),
    Stopped {
        client_id: DebugAdapterClientId,
        event: StoppedEvent,
        go_to_stack_frame: bool,
    },
    Thread((DebugAdapterClientId, ThreadEvent)),
    Continued((DebugAdapterClientId, ContinuedEvent)),
    Output((DebugAdapterClientId, OutputEvent)),
    Module((DebugAdapterClientId, ModuleEvent)),
    LoadedSource((DebugAdapterClientId, LoadedSourceEvent)),
    ClientShutdown(DebugAdapterClientId),
    CapabilitiesChanged(DebugAdapterClientId),
}

actions!(debug_panel, [ToggleFocus]);

#[derive(Debug, Default, Clone)]
pub struct ThreadState {
    pub status: ThreadStatus,
    // we update this value only once we stopped,
    // we will use this to indicated if we should show a warning when debugger thread was exited
    pub stopped: bool,
}

impl ThreadState {
    pub fn from_proto(thread_state: proto::DebuggerThreadState) -> Self {
        let status = ThreadStatus::from_proto(thread_state.thread_status());

        Self {
            status,
            stopped: thread_state.stopped,
        }
    }

    pub fn to_proto(&self) -> proto::DebuggerThreadState {
        let status = self.status.to_proto();

        proto::DebuggerThreadState {
            thread_status: status,
            stopped: self.stopped,
        }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThreadStatus {
    #[default]
    Running,
    Stopped,
    Exited,
    Ended,
}

impl ThreadStatus {
    pub fn from_proto(status: proto::DebuggerThreadStatus) -> Self {
        match status {
            proto::DebuggerThreadStatus::Running => Self::Running,
            proto::DebuggerThreadStatus::Stopped => Self::Stopped,
            proto::DebuggerThreadStatus::Exited => Self::Exited,
            proto::DebuggerThreadStatus::Ended => Self::Ended,
        }
    }

    pub fn to_proto(&self) -> i32 {
        match self {
            Self::Running => proto::DebuggerThreadStatus::Running.into(),
            Self::Stopped => proto::DebuggerThreadStatus::Stopped.into(),
            Self::Exited => proto::DebuggerThreadStatus::Exited.into(),
            Self::Ended => proto::DebuggerThreadStatus::Ended.into(),
        }
    }
}

pub struct DebugPanel {
    size: Pixels,
    pane: Entity<Pane>,
    focus_handle: FocusHandle,
    dap_store: Entity<DapStore>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
    message_queue: HashMap<DebugAdapterClientId, VecDeque<OutputEvent>>,
}

impl DebugPanel {
    pub fn new(
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let pane = cx.new(|cx| {
                let mut pane = Pane::new(
                    workspace.weak_handle(),
                    workspace.project().clone(),
                    Default::default(),
                    None,
                    gpui::NoAction.boxed_clone(),
                    window,
                    cx,
                );
                pane.set_can_split(None);
                pane.set_can_navigate(true, cx);
                pane.display_nav_history_buttons(None);
                pane.set_should_display_tab_bar(|_window, _cx| true);
                pane.set_close_pane_if_empty(false, cx);

                pane
            });

            let project = workspace.project().clone();
            let dap_store = project.read(cx).dap_store();

            let _subscriptions = vec![
                cx.observe(&pane, |_, _, cx| cx.notify()),
                cx.subscribe_in(&pane, window, Self::handle_pane_event),
            ];

            let dap_store = project.read(cx).dap_store();

            let mut debug_panel = Self {
                pane,
                size: px(300.),
                _subscriptions,
                focus_handle: cx.focus_handle(),
                message_queue: Default::default(),
                workspace: workspace.weak_handle(),
                dap_store: dap_store.clone(),
            };

            debug_panel
        })
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(|mut cx| async move {
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let debug_panel = DebugPanel::new(workspace, window, cx);

                cx.observe(&debug_panel, |_, debug_panel, cx| {
                    let (has_active_session, support_step_back) =
                        debug_panel.update(cx, |this, cx| {
                            this.active_debug_panel_item(cx)
                                .map(|item| {
                                    (
                                        true,
                                        item.update(cx, |this, cx| this.capabilities(cx))
                                            .supports_step_back
                                            .unwrap_or(false),
                                    )
                                })
                                .unwrap_or((false, false))
                        });

                    let filter = CommandPaletteFilter::global_mut(cx);
                    let debugger_action_types = [
                        TypeId::of::<Continue>(),
                        TypeId::of::<StepOver>(),
                        TypeId::of::<StepInto>(),
                        TypeId::of::<StepOut>(),
                        TypeId::of::<Stop>(),
                        TypeId::of::<Disconnect>(),
                        TypeId::of::<Pause>(),
                        TypeId::of::<Restart>(),
                        TypeId::of::<ToggleIgnoreBreakpoints>(),
                    ];

                    let step_back_action_type = [TypeId::of::<StepBack>()];

                    if has_active_session {
                        filter.show_action_types(debugger_action_types.iter());

                        if support_step_back {
                            filter.show_action_types(step_back_action_type.iter());
                        } else {
                            filter.hide_action_types(&step_back_action_type);
                        }
                    } else {
                        // show only the `debug: start`
                        filter.hide_action_types(&debugger_action_types);
                        filter.hide_action_types(&step_back_action_type);
                    }
                })
                .detach();

                debug_panel
            })
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn message_queue(&self) -> &HashMap<DebugAdapterClientId, VecDeque<OutputEvent>> {
        &self.message_queue
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn dap_store(&self) -> Entity<DapStore> {
        self.dap_store.clone()
    }

    pub fn active_debug_panel_item(&self, cx: &Context<Self>) -> Option<Entity<DebugPanelItem>> {
        self.pane
            .read(cx)
            .active_item()
            .and_then(|panel| panel.downcast::<DebugPanelItem>())
    }

    pub fn debug_panel_items_by_client(
        &self,
        client_id: &DebugAdapterClientId,
        cx: &Context<Self>,
    ) -> Vec<Entity<DebugPanelItem>> {
        self.pane
            .read(cx)
            .items()
            .filter_map(|item| item.downcast::<DebugPanelItem>())
            .filter(|item| &item.read(cx).client_id() == client_id)
            .map(|item| item.clone())
            .collect()
    }

    pub fn debug_panel_item_by_client(
        &self,
        client_id: DebugAdapterClientId,
        thread_id: ThreadId,
        cx: &mut Context<Self>,
    ) -> Option<Entity<DebugPanelItem>> {
        self.pane
            .read(cx)
            .items()
            .filter_map(|item| item.downcast::<DebugPanelItem>())
            .find(|item| {
                let item = item.read(cx);

                item.client_id() == client_id && item.thread_id() == thread_id
            })
    }

    fn handle_pane_event(
        &mut self,
        _: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::RemovedItem { item } => {
                let thread_panel = item.downcast::<DebugPanelItem>().unwrap();

                let thread_id = thread_panel.read(cx).thread_id();

                cx.notify();

                thread_panel.update(cx, |this, cx| {
                    this.session().update(cx, |state, cx| {
                        state.terminate_threads(Some(vec![thread_id; 1]), cx);
                    })
                });
            }
            pane::Event::Remove { .. } => cx.emit(PanelEvent::Close),
            pane::Event::ZoomIn => cx.emit(PanelEvent::ZoomIn),
            pane::Event::ZoomOut => cx.emit(PanelEvent::ZoomOut),
            pane::Event::AddItem { item } => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, self.pane.clone(), window, cx)
                    })
                    .ok();
            }
            pane::Event::ActivateItem { local, .. } => {
                if !local {
                    return;
                }

                if let Some(active_item) = self.pane.read(cx).active_item() {
                    if let Some(debug_item) = active_item.downcast::<DebugPanelItem>() {
                        debug_item.update(cx, |panel, cx| {
                            panel.go_to_current_stack_frame(window, cx);
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

impl EventEmitter<PanelEvent> for DebugPanel {}
impl EventEmitter<DebugPanelEvent> for DebugPanel {}
impl EventEmitter<project::Event> for DebugPanel {}

impl Focusable for DebugPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for DebugPanel {
    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.pane.clone())
    }

    fn persistent_name() -> &'static str {
        "DebugPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.size
    }

    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) {
        self.size = size.unwrap();
    }

    fn remote_id() -> Option<proto::PanelId> {
        Some(proto::PanelId::DebugPanel)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Debug)
    }

    fn icon_tooltip(&self, _window: &Window, cx: &App) -> Option<&'static str> {
        if DebuggerSettings::get_global(cx).button {
            Some("Debug Panel")
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

impl Render for DebugPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("DebugPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .map(|this| {
                if self.pane.read(cx).items_len() == 0 {
                    this.child(
                        h_flex().size_full().items_center().justify_center().child(
                            v_flex()
                                .gap_2()
                                .rounded_md()
                                .max_w_64()
                                .items_start()
                                .child(
                                    Label::new("You can create a debug task by creating a new task and setting the `type` key to `debug`")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    h_flex().w_full().justify_end().child(
                                        Button::new(
                                            "start-debugger",
                                            "Choose a debugger",
                                        )
                                        .label_size(LabelSize::Small)
                                        .on_click(move |_, window, cx| {
                                            window.dispatch_action(Box::new(Start), cx);
                                        })
                                    ),
                                ),
                        ),
                    )
                } else {
                    this.child(self.pane.clone())
                }
            })
            .into_any()
    }
}
